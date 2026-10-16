//! Content addressed cache of fetched boundary bytes, keyed by sha256 of the
//! URL. Two files per entry: `<key>.bin` and `<key>.meta.json` (url,
//! fetched_at, sha256 of the bin, verified on read). Written temp-then-rename,
//! bin before meta, so a meta file is the commit marker. Nothing here aborts a
//! run: every failure is an `Err(String)` the caller reports as `cache_error`.
//! Same layout as the Python cache, so directories are interchangeable.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::snapshot::instant::format_utc;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn cache_key(url: &str) -> String {
    hex(&Sha256::digest(url.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    fn paths(&self, url: &str) -> (PathBuf, PathBuf) {
        let key = cache_key(url);
        (
            self.dir.join(format!("{key}.bin")),
            self.dir.join(format!("{key}.meta.json")),
        )
    }

    /// Ok(None) on a miss (no meta file). Err on a corrupt or unreadable entry.
    /// Treat an Err the same as a miss -- re-fetch the URL -- and report it
    /// as `cache_error`; never serve the entry it names as good data.
    pub fn read(&self, url: &str) -> std::result::Result<Option<Vec<u8>>, String> {
        let (bin, meta) = self.paths(url);
        let meta_text = match std::fs::read_to_string(&meta) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", meta.display())),
        };
        let meta_json: serde_json::Value =
            serde_json::from_str(&meta_text).map_err(|e| format!("{}: {e}", meta.display()))?;
        let expected = meta_json["sha256"]
            .as_str()
            .ok_or_else(|| format!("{}: no sha256", meta.display()))?;
        let bytes = std::fs::read(&bin).map_err(|e| format!("{}: {e}", bin.display()))?;
        let actual = hex(&Sha256::digest(&bytes));
        if actual != expected {
            return Err(format!(
                "{}: sha256 mismatch (entry corrupt or truncated)",
                bin.display()
            ));
        }
        Ok(Some(bytes))
    }

    /// Two writers racing on the same URL with differing bytes can leave a
    /// mismatched bin/meta pair (one writer's bin next to the other's meta);
    /// `read` detects that via the sha256 check and reports it as an error
    /// rather than serving either writer's bytes as if they were the other's.
    pub fn write(&self, url: &str, bytes: &[u8]) -> std::result::Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let (bin, meta) = self.paths(url);
        atomic_write(&bin, bytes)?;
        let meta_json = serde_json::json!({
            "url": url,
            "fetched_at": format_utc(std::time::SystemTime::now()),
            "sha256": hex(&Sha256::digest(bytes)),
        });
        atomic_write(&meta, meta_json.to_string().as_bytes())
    }
}

/// Write to a sibling temp file unique to this process and call, then rename.
/// Two workers racing on the same entry each rename identical bytes; last wins.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::result::Result<(), String> {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}-{seq}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_sha256_of_the_url_and_matches_python() {
        // python3 -c "import hashlib;print(hashlib.sha256(b'https://a/b').hexdigest())"
        let expected = "4148a87e16a32df5f7bd99effa01d69d8371d4e077fa2fca9f1cfca818ae02f8";
        assert_eq!(cache_key("https://a/b"), expected);
        assert_ne!(cache_key("https://a/b"), cache_key("https://a/c"));
    }

    #[test]
    fn write_then_read_round_trips_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path());
        cache.write("https://a/b", b"geo").unwrap();
        assert_eq!(cache.read("https://a/b").unwrap(), Some(b"geo".to_vec()));
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(
            names.iter().any(|n| n.ends_with(".bin"))
                && names.iter().any(|n| n.ends_with(".meta.json"))
        );
        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                dir.path()
                    .join(format!("{}.meta.json", cache_key("https://a/b"))),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(meta["url"], "https://a/b");
        assert!(meta["fetched_at"].as_str().unwrap().ends_with('Z'));
        assert_eq!(meta["sha256"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn a_miss_is_none_and_a_corrupt_entry_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path());
        assert_eq!(cache.read("https://a/none").unwrap(), None);
        cache.write("https://a/b", b"geo").unwrap();
        std::fs::write(
            dir.path().join(format!("{}.bin", cache_key("https://a/b"))),
            b"tampered",
        )
        .unwrap();
        let err = cache.read("https://a/b").unwrap_err();
        assert!(err.contains("sha256"), "{err}");
    }

    #[test]
    fn a_bin_without_meta_is_a_miss_and_a_meta_without_bin_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path());
        std::fs::write(dir.path().join(format!("{}.bin", cache_key("u"))), b"x").unwrap();
        assert_eq!(cache.read("u").unwrap(), None);
        cache.write("v", b"y").unwrap();
        std::fs::remove_file(dir.path().join(format!("{}.bin", cache_key("v")))).unwrap();
        assert!(cache.read("v").is_err());
    }

    #[test]
    fn unwritable_dir_is_an_error_string_not_a_panic() {
        let f = tempfile::NamedTempFile::new().unwrap();
        // A file where a directory is needed.
        let cache = Cache::new(&f.path().join("sub"));
        assert!(cache.write("u", b"x").is_err());
    }

    #[test]
    fn concurrent_writes_of_different_bytes_to_the_same_url_are_never_torn() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path());
        let payloads: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i; 4096]).collect();
        std::thread::scope(|s| {
            for payload in &payloads {
                let cache = cache.clone();
                s.spawn(move || cache.write("https://a/same", payload).unwrap());
            }
        });
        match cache.read("https://a/same") {
            Ok(Some(bytes)) => assert!(
                payloads.contains(&bytes),
                "read bytes did not match any single writer's payload"
            ),
            Err(e) => assert!(e.contains("sha256"), "{e}"),
            Ok(None) => panic!("expected a hit or a sha256 error, got a miss"),
        }
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            2,
            "no temp files left"
        );
    }
}
