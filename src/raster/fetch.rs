//! Where the raster comes from: a local file, or an http(s) URL fetched once
//! into `SNAPSHOT/rasters/` through the same content-addressed `Cache` the
//! boundary fetch uses (sha256 of the URL; bytes verified on read), so
//! re-runs and other years never re-download a country raster. The cache key
//! is the whole URL, so a presigned URL that changes per run never hits the
//! cache.

use std::path::Path;
use std::time::Duration;

use crate::error::{PipesError, Result};
use crate::extract::cache::Cache;
use crate::extract::client::{Auth, FhirClient};

/// Classic and BigTIFF headers, both byte orders.
const TIFF_MAGIC: [&[u8; 4]; 4] = [b"II*\0", b"MM\0*", b"II+\0", b"MM\0+"];

pub fn looks_like_tiff(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && TIFF_MAGIC.iter().any(|m| &bytes[..4] == *m)
}

#[derive(Debug)]
pub struct RasterBytes {
    pub bytes: Vec<u8>,
    /// File name or last URL segment: for error messages and provenance text.
    pub label: String,
    /// Served from the cache rather than fetched or read.
    pub cached: bool,
    /// Cache faults encountered on the way; the caller reports them as
    /// `cache_error`. Empty for local files and clean hits.
    pub cache_errors: Vec<String>,
}

pub fn is_url(source: &str) -> bool {
    let scheme = source.get(..8).unwrap_or(source).to_ascii_lowercase();
    scheme.starts_with("http://") || scheme.starts_with("https://")
}

pub fn label_of(source: &str) -> String {
    let path = source
        .split(['?', '#'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(source);
    path.rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Read `source` into memory. A URL goes through `cache_dir`; cache faults
/// are collected on the returned value and treated as a miss, never as data.
pub fn load(
    source: &str,
    cache_dir: &Path,
    retries: usize,
    timeout: Duration,
) -> Result<RasterBytes> {
    let label = label_of(source);
    if !is_url(source) {
        if source.contains("://") {
            return Err(PipesError::Usage(format!(
                "{source}: only http(s) URLs and local paths are supported"
            )));
        }
        let path = Path::new(source);
        let bytes = std::fs::read(path).map_err(|e| PipesError::io(path, e))?;
        return Ok(RasterBytes {
            bytes,
            label,
            cached: false,
            cache_errors: Vec::new(),
        });
    }
    let mut cache_errors = Vec::new();
    let cache = Cache::new(cache_dir);
    match cache.read(source) {
        Ok(Some(bytes)) => {
            return Ok(RasterBytes {
                bytes,
                label,
                cached: true,
                cache_errors,
            });
        }
        Ok(None) => {}
        Err(e) => cache_errors.push(format!("read: {e}")),
    }
    let client = FhirClient::new(Auth::None, retries, timeout)?;
    eprintln!("raster: fetching {label} …");
    let fetched = client
        .get(source)
        .map_err(|e| PipesError::Environment(format!("{source}: {e}")))?;
    if !looks_like_tiff(&fetched.body) {
        return Err(PipesError::Environment(format!(
            "{source}: response is not a TIFF ({} bytes); nothing cached",
            fetched.body.len()
        )));
    }
    if let Err(e) = cache.write(source, &fetched.body) {
        cache_errors.push(format!("write: {e}"));
    }
    Ok(RasterBytes {
        bytes: fetched.body,
        label,
        cached: false,
        cache_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::geotiff::FIXTURE;
    use httptest::{Expectation, Server, matchers::*, responders::*};

    #[test]
    fn labels_are_the_last_path_segment() {
        assert_eq!(
            label_of("https://h/x/nga_pop_2026_CN_100m_cog.tif"),
            "nga_pop_2026_CN_100m_cog.tif"
        );
        assert_eq!(label_of("/data/a.tif"), "a.tif");
        assert_eq!(label_of("a.tif"), "a.tif");
        assert_eq!(
            label_of("https://h/nga.tif?X-Amz-Signature=abc#frag"),
            "nga.tif"
        );
        assert!(is_url("https://h/a.tif") && is_url("http://h/a.tif") && !is_url("/h/a.tif"));
        assert!(is_url("HTTPS://h/a.tif"));
    }

    #[test]
    fn a_local_file_is_read_directly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pop.tif");
        std::fs::write(&path, FIXTURE).unwrap();
        let got = load(
            path.to_str().unwrap(),
            &dir.path().join("cache"),
            1,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(got.bytes, FIXTURE);
        assert_eq!(got.label, "pop.tif");
        assert!(!got.cached);
        assert!(got.cache_errors.is_empty());
        assert!(
            !dir.path().join("cache").exists(),
            "no cache for local files"
        );
    }

    #[test]
    fn a_url_is_fetched_once_then_served_from_the_cache() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/nga.tif"))
                .times(1)
                .respond_with(status_code(200).body(FIXTURE.to_vec())),
        );
        let url = server.url("/nga.tif").to_string();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("rasters");

        let first = load(&url, &cache, 1, Duration::from_secs(5)).unwrap();
        assert_eq!(first.bytes, FIXTURE);
        assert_eq!(first.label, "nga.tif");
        assert!(!first.cached);
        assert!(first.cache_errors.is_empty());
        assert_eq!(
            std::fs::read_dir(&cache).unwrap().count(),
            2,
            ".bin and .meta.json"
        );

        let second = load(&url, &cache, 1, Duration::from_secs(5)).unwrap();
        assert_eq!(second.bytes, FIXTURE);
        assert!(second.cached);
    }

    #[test]
    fn a_missing_url_is_an_environment_error() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/none.tif"))
                .respond_with(status_code(404)),
        );
        let err = load(
            &server.url("/none.tif").to_string(),
            &tempfile::tempdir().unwrap().path().join("c"),
            1,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[test]
    fn a_missing_local_file_is_an_io_error_naming_the_path() {
        let err = load(
            "/definitely/not/here.tif",
            Path::new("/unused"),
            1,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("/definitely/not/here.tif"),
            "{err}"
        );
    }

    #[test]
    fn a_corrupt_cache_entry_is_refetched_and_repaired() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/nga.tif"))
                .times(2)
                .respond_with(status_code(200).body(FIXTURE.to_vec())),
        );
        let url = server.url("/nga.tif").to_string();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("rasters");
        load(&url, &cache, 1, Duration::from_secs(5)).unwrap();
        let bin = std::fs::read_dir(&cache)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "bin"))
            .unwrap();
        std::fs::write(&bin, b"tampered").unwrap();

        let again = load(&url, &cache, 1, Duration::from_secs(5)).unwrap();
        assert_eq!(again.bytes, FIXTURE, "never serve the corrupt bytes");
        assert!(!again.cached, "a corrupt entry is a miss, not a hit");
        assert_eq!(again.cache_errors.len(), 1, "{:?}", again.cache_errors);
        assert!(
            again.cache_errors[0].contains("sha256"),
            "{:?}",
            again.cache_errors
        );
        assert_eq!(std::fs::read(&bin).unwrap(), FIXTURE, "entry rewritten");
    }

    #[test]
    fn a_non_tiff_response_is_an_error_and_is_not_cached() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/oops.tif"))
                .respond_with(status_code(200).body("<html>captive portal</html>")),
        );
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("rasters");
        let err = load(
            &server.url("/oops.tif").to_string(),
            &cache,
            1,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("not a TIFF"), "{err}");
        assert!(!cache.exists() || std::fs::read_dir(&cache).unwrap().count() == 0);
    }

    #[test]
    fn an_unsupported_scheme_is_a_usage_error() {
        let err = load(
            "s3://bucket/nga.tif",
            Path::new("/unused"),
            1,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}
