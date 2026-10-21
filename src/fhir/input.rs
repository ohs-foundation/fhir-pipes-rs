//! Reading FHIR resources from files: a file or a directory of `.json` and
//! `.ndjson` files (gzip too), each a resource, an array of resources or a
//! Bundle. `views run --input` and `refresh`'s file source read through
//! this.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::error::usage;
use crate::fhir::chunks;
use crate::fhir::ndjson::{Splitter, is_gz, read_text};

/// Read `files` in order and hand their resources to `f` a chunk at a time:
/// an NDJSON file (`is_ndjson`) a chunk of lines at a time,
/// parsed on `threads` threads, other files whole, several small ones
/// together, up to a chunk's worth of resources. A Bundle or an array is
/// handed on as its resources. Returns the number of resources.
pub(crate) fn for_each_chunk(
    files: &[PathBuf],
    threads: usize,
    keep_bundles: bool,
    mut f: impl FnMut(Vec<serde_json::Value>) -> Result<()>,
) -> Result<usize> {
    let mut count = 0;
    // Resources of JSON files not yet handed on.
    let mut pending = Vec::new();
    for file in files {
        if is_ndjson(file) {
            if !pending.is_empty() {
                f(std::mem::take(&mut pending))?;
            }
            for chunk in chunks::open(file)? {
                let mut resources = Vec::new();
                // A line that is not JSON is the input's fault.
                for value in chunk?.values(threads).map_err(usage)? {
                    push_resource(value, &mut resources, keep_bundles);
                }
                count += resources.len();
                f(resources)?;
            }
        } else {
            let before = pending.len();
            load_json_file(file, &mut pending, keep_bundles)?;
            count += pending.len() - before;
            if pending.len() >= chunks::CHUNK_LINES {
                f(std::mem::take(&mut pending))?;
            }
        }
    }
    if !pending.is_empty() {
        f(pending)?;
    }
    Ok(count)
}

/// The files `path` names: itself if it is a file, else the resource files
/// under it (`collect_resource_files`), sorted by path. `refresh` reads its
/// file source through this.
pub(crate) fn resource_files(path: &Path) -> Result<Vec<PathBuf>> {
    if !path.is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut files = Vec::new();
    collect_resource_files(path, &mut files)?;
    files.sort();
    check_no_gz_copies(&files)?;
    Ok(files)
}

/// The extension that says what a resource file holds, past a `.gz`:
/// `ndjson` for both `a.ndjson` and `a.ndjson.gz`.
fn content_extension(path: &Path) -> Option<&OsStr> {
    if is_gz(path) {
        Path::new(path.file_stem()?).extension()
    } else {
        path.extension()
    }
}

/// Whether `path` is read as NDJSON: `.ndjson` or `.ndjson.gz`. Any other
/// file is read as one JSON value.
pub(crate) fn is_ndjson(path: &Path) -> bool {
    content_extension(path).is_some_and(|x| x == "ndjson")
}

/// Whether a directory's or a pattern's file is read as resources:
/// `.json`, `.ndjson`, `.json.gz` or `.ndjson.gz`.
pub(crate) fn is_resource_file(path: &Path) -> bool {
    content_extension(path).is_some_and(|x| x == "json" || x == "ndjson")
}

/// An error when `files` holds both `X` and `X.gz`. Both would be read, and
/// they most likely hold the same resources (`gzip -k` leaves the original),
/// which would give every row twice.
pub(crate) fn check_no_gz_copies(files: &[PathBuf]) -> Result<()> {
    let plain: HashSet<&Path> = files
        .iter()
        .filter(|f| !is_gz(f))
        .map(PathBuf::as_path)
        .collect();
    for gz in files.iter().filter(|f| is_gz(f)) {
        let original = gz.with_extension("");
        if plain.contains(original.as_path()) {
            return Err(usage(anyhow::anyhow!(
                "{} and {} are both inputs, and would both be read; remove one",
                original.display(),
                gz.display()
            )));
        }
    }
    Ok(())
}

/// The `.json` and `.ndjson` files under `dir`, and the same compressed
/// (`.json.gz`, `.ndjson.gz`), in subdirectories too.
/// Hidden files and directories (`.export-state.json`) are skipped, and so
/// are symlinked directories, which could lead back up the tree and read the
/// same files again. Symlinked files are read.
fn collect_resource_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?
    {
        let entry = entry.with_context(|| format!("reading directory {}", dir.display()))?;
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        // file_type() does not follow a symlink; is_dir() and is_file() do.
        let kind = entry
            .file_type()
            .with_context(|| format!("reading {}", path.display()))?;
        if kind.is_dir() {
            collect_resource_files(&path, out)?;
        } else if (kind.is_file() || (kind.is_symlink() && path.is_file()))
            && is_resource_file(&path)
        {
            out.push(path);
        }
    }
    Ok(())
}

/// The resources in one JSON file, a resource, an array or a Bundle
/// (`push_resource`), appended to `out`. A `.gz` file is read decompressed.
pub(crate) fn load_json_file(
    path: &Path,
    out: &mut Vec<serde_json::Value>,
    keep_bundles: bool,
) -> Result<()> {
    let read = || -> io::Result<String> {
        let mut text = String::new();
        read_text(fs::File::open(path)?, is_gz(path)).read_to_string(&mut text)?;
        Ok(text)
    };
    let text = read().map_err(|e| chunks::read_error(e, format!("reading {}", path.display())))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))
        .map_err(usage)?;
    push_resource(value, out, keep_bundles);
    Ok(())
}

/// Resource text of unknown shape (stdin has no extension to go by): one JSON
/// value if it parses whole, otherwise NDJSON.
pub(crate) fn parse_resource_text(
    text: &str,
    out: &mut Vec<serde_json::Value>,
    keep_bundles: bool,
) -> Result<()> {
    let whole = text.strip_prefix('\u{feff}').unwrap_or(text);
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(whole) {
        push_resource(value, out, keep_bundles);
        return Ok(());
    }
    let mut lines = Splitter::new(text.as_bytes());
    let mut buf = Vec::new();
    while let Some(span) = lines.next_into(&mut buf)? {
        let value: serde_json::Value = serde_json::from_slice(&buf[span.start..span.end])
            .with_context(|| format!("parsing line {}", span.number))?;
        push_resource(value, out, keep_bundles);
        buf.clear();
    }
    Ok(())
}

/// A Bundle becomes its entries' resources, an array its elements, anything
/// else one resource.
///
/// A reference to another entry of the same Bundle by its `urn:` fullUrl
/// (`urn:uuid:…`, as in a transaction Bundle) is rewritten to that entry's
/// `Type/id`, as a server would store it, so `getReferenceKey()` and joins
/// work on Bundle files. An entry without an id is left as is.
///
/// With `keep_bundles` (a view reads Bundles) the Bundle itself is kept as
/// well, entries and all.
pub(crate) fn push_resource(
    value: serde_json::Value,
    out: &mut Vec<serde_json::Value>,
    keep_bundles: bool,
) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                push_resource(item, out, keep_bundles);
            }
        }
        serde_json::Value::Object(mut map)
            if map.get("resourceType").and_then(|v| v.as_str()) == Some("Bundle") =>
        {
            if keep_bundles {
                out.push(serde_json::Value::Object(map.clone()));
            }
            let Some(serde_json::Value::Array(entries)) = map.remove("entry") else {
                return;
            };
            let urns = bundle_urns(&entries);
            for entry in entries {
                if let serde_json::Value::Object(mut entry) = entry
                    && let Some(mut resource) = entry.remove("resource")
                {
                    if !urns.is_empty() {
                        resolve_urns(&mut resource, &urns);
                    }
                    push_resource(resource, out, keep_bundles);
                }
            }
        }
        other => out.push(other),
    }
}

/// `urn:` fullUrl → `Type/id` for the entries of a Bundle that have both.
fn bundle_urns(entries: &[serde_json::Value]) -> HashMap<String, String> {
    entries
        .iter()
        .filter_map(|entry| {
            let full_url = entry.get("fullUrl")?.as_str()?;
            if !full_url.starts_with("urn:") {
                return None;
            }
            let resource = entry.get("resource")?;
            let ty = resource.get("resourceType")?.as_str()?;
            let id = resource.get("id")?.as_str()?;
            Some((full_url.to_string(), format!("{ty}/{id}")))
        })
        .collect()
}

/// Rewrite every `reference` in `value` that is a key of `urns`. A Bundle
/// inside `value` is left alone: it resolves its own entries when it is read.
fn resolve_urns(value: &mut serde_json::Value, urns: &HashMap<String, String>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("resourceType").and_then(|v| v.as_str()) == Some("Bundle") {
                return;
            }
            if let Some(serde_json::Value::String(reference)) = map.get_mut("reference")
                && let Some(resolved) = urns.get(reference.as_str())
            {
                *reference = resolved.clone();
            }
            for child in map.values_mut() {
                resolve_urns(child, urns);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                resolve_urns(item, urns);
            }
        }
        _ => {}
    }
}
