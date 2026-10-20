//! FHIR packages and directories of FHIR JSON, read for the resources in
//! them: `package.tgz` files (a path or an https URL), unpacked packages, a
//! package in the local FHIR package cache by `name#version`, and directories
//! such as SUSHI's `fsh-generated/resources`.

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
