//! FHIR packages and directories of FHIR JSON, read for the resources in
//! them: `package.tgz` files (a path or an https URL), unpacked packages, a
//! package in the local FHIR package cache by `name#version`, and directories
//! such as SUSHI's `fsh-generated/resources`. `Index` keeps the resources
//! of some types by canonical URL; `terminology`, `profile check` and
//! `views lint --package` read packages through it.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{PipesError, Result};

/// Call `visit` with the name and parsed JSON of every JSON file `spec`
/// names: an https URL or path to a package `.tgz`, a directory (read with
/// its subdirectories), or `name#version` for a package in the local FHIR
/// package cache (`~/.fhir/packages`). `flag` names the option `spec` came
/// from, for messages. A file that is not UTF-8 JSON (a byte order mark is
/// allowed) is skipped, and the files skipped are counted in one warning: a
/// resource missing that way would otherwise surface as "not in the
/// packages".
pub fn read_json_files(
    spec: &str,
    flag: &str,
    mut visit: impl FnMut(&str, serde_json::Value),
) -> Result<()> {
    let mut skipped: Vec<String> = Vec::new();
    let mut each = |name: &str, text: std::result::Result<&str, String>| {
        let parsed = text.and_then(|t| {
            serde_json::from_str(t.strip_prefix('\u{feff}').unwrap_or(t)).map_err(|e| e.to_string())
        });
        match parsed {
            Ok(v) => visit(name, v),
            Err(why) => skipped.push(format!("{name}: {why}")),
        }
    };
    let read = read_any(spec, flag, &mut each);
    if let Some(first) = skipped.first() {
        eprintln!(
            "warning: {flag} {spec}: {} JSON file(s) could not be read and were skipped, the first {first}",
            skipped.len()
        );
    }
    read
}

/// The resources of the types asked for in a set of packages, by type and
/// canonical URL, every version kept: the same URL and version read twice
/// (from two packages, or two copies in a directory) is kept once. The
/// packages' own `package.json` manifests are kept too.
#[derive(Debug, Default)]
pub struct Index {
    types: Vec<String>,
    resources: HashMap<String, HashMap<String, Vec<serde_json::Value>>>,
    manifests: Vec<serde_json::Value>,
}

impl Index {
    /// An empty index that keeps resources of `types`.
    pub fn new(types: &[&str]) -> Self {
        Self {
            types: types.iter().map(|t| t.to_string()).collect(),
            ..Self::default()
        }
    }

    /// The resources of `types` in every package of `specs` (as
    /// `read_json_files` reads them; `flag` names the option for messages).
    pub fn load(specs: &[String], flag: &str, types: &[&str]) -> Result<Self> {
        let mut index = Self::new(types);
        for spec in specs {
            read_json_files(spec, flag, |name, v| index.add(name, v))?;
        }
        Ok(index)
    }

    /// Keep `v`, read from the file `name`, if it is a package manifest or a
    /// resource of one of the index's types with a URL.
    pub fn add(&mut self, name: &str, v: serde_json::Value) {
        if v.get("resourceType").is_none() {
            if Path::new(name)
                .file_name()
                .is_some_and(|n| n == "package.json")
            {
                self.manifests.push(v);
            }
            return;
        }
        let Some(kind) = v["resourceType"]
            .as_str()
            .filter(|t| self.types.iter().any(|k| k == t))
        else {
            return;
        };
        let Some(url) = v["url"].as_str().map(str::to_string) else {
            return;
        };
        let versions = self
            .resources
            .entry(kind.to_string())
            .or_default()
            .entry(url)
            .or_default();
        if !versions.iter().any(|r| r["version"] == v["version"]) {
            versions.push(v);
        }
    }

    /// Every version of the `resource_type` at `url`, in the order read.
    pub fn versions(&self, resource_type: &str, url: &str) -> &[serde_json::Value] {
        self.resources
            .get(resource_type)
            .and_then(|m| m.get(url))
            .map_or(&[], Vec::as_slice)
    }

    /// The resources of `resource_type`, by URL, each URL's versions in the
    /// order read, taken out of the index.
    pub fn take_type(&mut self, resource_type: &str) -> HashMap<String, Vec<serde_json::Value>> {
        self.resources.remove(resource_type).unwrap_or_default()
    }

    /// The `package.json` manifests read.
    pub fn manifests(&self) -> &[serde_json::Value] {
        &self.manifests
    }
}

type Visit<'a> = dyn FnMut(&str, std::result::Result<&str, String>) + 'a;

fn read_any(spec: &str, flag: &str, visit: &mut Visit) -> Result<()> {
    if spec.starts_with("http://") {
        return Err(PipesError::Usage(format!(
            "{flag} {spec}: packages are fetched over https only"
        )));
    }
    if spec.starts_with("https://") {
        let bytes = crate::profile::fetch_bytes(spec, flag, Duration::from_secs(120))?;
        return read_tgz(&bytes, spec, flag, visit);
    }
    let path = Path::new(spec);
    if path.is_dir() {
        read_dir(path, visit)
    } else if path.is_file() {
        let bytes = std::fs::read(path).map_err(|e| PipesError::io(path, e))?;
        read_tgz(&bytes, spec, flag, visit)
    } else if let Some(dir) = cached(spec) {
        read_dir(&dir, visit)
    } else {
        Err(PipesError::Usage(format!(
            "{flag} {spec}: not a file, directory, https URL or name#version in the \
             FHIR package cache"
        )))
    }
}

/// The package `name` at `version`, as a spec for `read_json_files`:
/// `name#version` when it is in the local FHIR package cache, otherwise its
/// download from the FHIR package registry.
#[cfg(feature = "fhir-packages")]
pub fn by_version(name: &str, version: &str) -> String {
    let spec = format!("{name}#{version}");
    if cached(&spec).is_some() {
        return spec;
    }
    format!("{REGISTRY}/{name}/{version}")
}

/// The FHIR package registry.
#[cfg(feature = "fhir-packages")]
const REGISTRY: &str = "https://packages.fhir.org";

/// The unpacked package `name#version` names in the local FHIR package cache,
/// where the IG publisher, SUSHI and the validator keep packages.
fn cached(spec: &str) -> Option<PathBuf> {
    if !spec.contains('#') || spec.contains('/') {
        return None;
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let dir = PathBuf::from(home)
        .join(".fhir/packages")
        .join(spec)
        .join("package");
    dir.is_dir().then_some(dir)
}

/// The largest JSON file read from a package, and the most in all: the
/// biggest packages (hl7.terminology) hold a few hundred MB.
const MAX_ENTRY: u64 = 512 << 20;
const MAX_TOTAL: u64 = 4 << 30;

/// A gzipped tar of JSON files, as the IG publisher writes `package.tgz`.
fn read_tgz(bytes: &[u8], spec: &str, flag: &str, visit: &mut Visit) -> Result<()> {
    read_tgz_up_to(bytes, spec, flag, visit, (MAX_ENTRY, MAX_TOTAL))
}

fn read_tgz_up_to(
    bytes: &[u8],
    spec: &str,
    flag: &str,
    visit: &mut Visit,
    (max_entry, max_total): (u64, u64),
) -> Result<()> {
    let bad =
        |e: std::io::Error| PipesError::Usage(format!("{flag} {spec}: not a FHIR package: {e}"));
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut total = 0u64;
    for entry in archive.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        let name = entry.path().map_err(bad)?.to_string_lossy().into_owned();
        if !name.ends_with(".json") {
            continue;
        }
        // Each entry's size is in its tar header, read before its content:
        // a few MB of gzip can claim, and hold, far more.
        let size = entry.size();
        total += size;
        if size > max_entry || total > max_total {
            return Err(PipesError::Usage(format!(
                "{flag} {spec}: {name} is {} MB, and the JSON files {} MB so far; over the \
                 {} MB per file or {} MB in all a FHIR package is read up to",
                size >> 20,
                total >> 20,
                max_entry >> 20,
                max_total >> 20
            )));
        }
        let mut text = String::new();
        match entry.read_to_string(&mut text) {
            Ok(_) => visit(&name, Ok(&text)),
            Err(e) => visit(&name, Err(e.to_string())),
        }
    }
    Ok(())
}

fn read_dir(dir: &Path, visit: &mut Visit) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| PipesError::io(dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| PipesError::io(dir, e))?;
        let path = entry.path();
        // Not through a symlink: a link back up the tree would recurse until
        // the stack ran out.
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            read_dir(&path, visit)?;
        } else if path.extension().is_some_and(|e| e == "json") {
            match std::fs::read_to_string(&path) {
                Ok(text) => visit(&path.to_string_lossy(), Ok(&text)),
                Err(e) => visit(&path.to_string_lossy(), Err(e.to_string())),
            }
        }
    }
    Ok(())
}

/// A package.tgz holding `files` under `package/`.
#[cfg(test)]
pub(crate) fn tgz(files: &[(&str, &str)]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    for (name, text) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(text.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, format!("package/{name}"), text.as_bytes())
            .unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_package_past_the_size_limits_is_refused() {
        let bytes = tgz(&[("a.json", "{}"), ("b.json", "[1, 2, 3]")]);
        let read = |limits| read_tgz_up_to(&bytes, "p.tgz", "--package", &mut |_, _| {}, limits);
        read((1 << 20, 1 << 20)).unwrap();
        assert!(
            read((4, 1 << 20))
                .unwrap_err()
                .to_string()
                .contains("b.json")
        );
        assert!(read((1 << 20, 6)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.json"), "{}").unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
        let mut seen = 0;
        read_dir(dir.path(), &mut |_, _| seen += 1).unwrap();
        assert_eq!(seen, 1);
    }

    #[test]
    fn a_bom_is_allowed_and_a_broken_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.tgz");
        std::fs::write(
            &path,
            tgz(&[
                ("a.json", "\u{feff}{\"id\":\"a\"}"),
                ("b.json", "{not json"),
            ]),
        )
        .unwrap();
        let mut ids = Vec::new();
        read_json_files(path.to_str().unwrap(), "--package", |_, v| {
            ids.push(v["id"].as_str().unwrap().to_string())
        })
        .unwrap();
        assert_eq!(ids, ["a"]);
    }

    #[test]
    fn the_index_keeps_each_version_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.tgz");
        let vs = |version: &str| {
            format!(r#"{{"resourceType":"ValueSet","url":"http://x/vs","version":"{version}"}}"#)
        };
        std::fs::write(
            &path,
            tgz(&[
                ("package.json", r#"{"name":"p","version":"1"}"#),
                ("a.json", &vs("1")),
                ("b.json", &vs("2")),
                ("c.json", &vs("1")),
                ("d.json", r#"{"resourceType":"Patient","url":"http://x/p"}"#),
                ("e.json", r#"{"resourceType":"ValueSet"}"#),
            ]),
        )
        .unwrap();
        let spec = path.to_str().unwrap().to_string();
        let index = Index::load(&[spec.clone(), spec], "--package", &["ValueSet"]).unwrap();
        let versions: Vec<&str> = index
            .versions("ValueSet", "http://x/vs")
            .iter()
            .map(|v| v["version"].as_str().unwrap())
            .collect();
        assert_eq!(versions, ["1", "2"]);
        assert_eq!(index.manifests().len(), 2);
        let mut index = index;
        assert_eq!(index.take_type("ValueSet").len(), 1);
        assert!(index.take_type("Patient").is_empty());
        assert!(index.versions("ValueSet", "http://x/vs").is_empty());
    }

    #[test]
    fn not_a_package_is_a_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.tgz");
        std::fs::write(&path, b"not gzip").unwrap();
        let err = read_json_files(path.to_str().unwrap(), "--ig", |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("not a FHIR package"), "{err}");
    }

    #[test]
    fn a_name_and_version_is_looked_up_in_the_package_cache() {
        assert_eq!(cached("hl7.fhir.r4.core"), None, "no version");
        assert_eq!(cached("./a#b"), None, "a path");
    }
}
