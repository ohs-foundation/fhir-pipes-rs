//! FHIR packages and directories of FHIR JSON, read for the resources in
//! them: `package.tgz` files (a path or an https URL), unpacked packages, a
//! package in the local FHIR package cache by `name#version`, and directories
//! such as SUSHI's `fsh-generated/resources`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{PipesError, Result};

/// Call `visit` with the name and text of every JSON file `spec` names: an
/// https URL or path to a package `.tgz`, a directory (read with its
/// subdirectories), or `name#version` for a package in the local FHIR
/// package cache (`~/.fhir/packages`). `flag` names the option `spec` came
/// from, for messages.
pub fn read_json_files(spec: &str, flag: &str, mut visit: impl FnMut(&str, &str)) -> Result<()> {
    if spec.starts_with("http://") {
        return Err(PipesError::Usage(format!(
            "{flag} {spec}: packages are fetched over https only"
        )));
    }
    if spec.starts_with("https://") {
        let bytes = crate::profile::fetch_bytes(spec, flag, Duration::from_secs(120))?;
        return read_tgz(&bytes, spec, flag, &mut visit);
    }
    let path = Path::new(spec);
    if path.is_dir() {
        read_dir(path, &mut visit)
    } else if path.is_file() {
        let bytes = std::fs::read(path).map_err(|e| PipesError::io(path, e))?;
        read_tgz(&bytes, spec, flag, &mut visit)
    } else if let Some(dir) = cached(spec) {
        read_dir(&dir, &mut visit)
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

/// A gzipped tar of JSON files, as the IG publisher writes `package.tgz`.
fn read_tgz(
    bytes: &[u8],
    spec: &str,
    flag: &str,
    visit: &mut impl FnMut(&str, &str),
) -> Result<()> {
    let bad =
        |e: std::io::Error| PipesError::Usage(format!("{flag} {spec}: not a FHIR package: {e}"));
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    for entry in archive.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        let name = entry.path().map_err(bad)?.to_string_lossy().into_owned();
        if !name.ends_with(".json") {
            continue;
        }
        let mut text = String::new();
        if entry.read_to_string(&mut text).is_ok() {
            visit(&name, &text);
        }
    }
    Ok(())
}

fn read_dir(dir: &Path, visit: &mut impl FnMut(&str, &str)) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| PipesError::io(dir, e))?;
    for entry in entries {
        let path = entry.map_err(|e| PipesError::io(dir, e))?.path();
        if path.is_dir() {
            read_dir(&path, visit)?;
        } else if path.extension().is_some_and(|e| e == "json")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            visit(&path.to_string_lossy(), &text);
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
