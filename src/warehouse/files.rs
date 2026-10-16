//! The file source: `[source] files` or `refresh --input`. The resources come
//! from NDJSON, Bundle and resource files, read with the reader `views run
//! --input` uses, instead of from a FHIR server, which is never contacted.
//!
//!   split      every resource into raw/<name>/<Type>.ndjson, one per id, as an
//!              export would have written them; the views step then reads them
//!              as it reads a download
//!   locations  the Location and Organization files among them into the
//!              snapshot raw/<name>/locations, transformed as after an extract

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::cli::{DEFAULT_PARTITION_BY, DEFAULT_ROW_GROUP_SIZE, TransformArgs};
use crate::snapshot::instant::{format_utc, parse_instant};
use crate::snapshot::{Snapshot, State};

use super::config::Settings;

/// The `server` of a Location snapshot built from files.
pub const FILES: &str = "files";

/// The files each input names, in order: a file as itself, a directory as
/// the resource files under it (`views run --input`'s rules: `.json` and
/// `.ndjson`, subdirectories too, hidden files skipped), a glob pattern as its
/// matches, sorted. A pattern's matching files must be `.json` or `.ndjson`;
/// a pattern that matches nothing, or a path that does not exist, is an
/// error. A file named twice is read once, where it first appears.
pub fn expand(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for input in inputs {
        files.extend(expand_one(input)?);
    }
    let mut seen = std::collections::HashSet::new();
    files.retain(|f| seen.insert(f.clone()));
    Ok(files)
}

fn expand_one(input: &Path) -> Result<Vec<PathBuf>> {
    let text = input.to_string_lossy();
    if !text.contains(['*', '?', '[']) {
        if !input.exists() {
            bail!("input {} not found", input.display());
        }
        return crate::views::resource_files(input);
    }
    // Hidden files are skipped here as in a directory.
    let options = glob::MatchOptions {
        require_literal_leading_dot: true,
        ..Default::default()
    };
    let mut matches: Vec<PathBuf> = glob::glob_with(&text, options)
        .with_context(|| format!("input pattern {text}"))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("input pattern {text}"))?;
    matches.sort();
    if matches.is_empty() {
        bail!("input pattern {text} matches nothing");
    }
    let mut files = Vec::new();
    for m in matches {
        if m.is_dir() {
            files.extend(crate::views::resource_files(&m)?);
        } else if m.extension().is_some_and(|x| x == "json" || x == "ndjson") {
            files.push(m);
        }
    }
    Ok(files)
}

/// One type's resources, in the order first seen, one per id.
#[derive(Default)]
struct Resources {
    list: Vec<Value>,
    by_id: HashMap<String, usize>,
    duplicates: usize,
}

/// Read every input file and write each resource type to
/// raw/<name>/<Type>.ndjson, written beside it and renamed into place. A
/// resource seen twice (same type and id) is kept once: the one with the later
/// `meta.lastUpdated`; on a tie, or when neither has one, the one read later.
/// A `<Type>.ndjson` left from an earlier run, of a type the files no longer
/// have, is removed, so the views do not read it.
///
/// The directory can hold a server's download (`server.name` names both), so
/// its export state goes first: the watermarks in it describe the server's
/// files, not these, and a later server refresh must download in full.
pub fn split(s: &Settings) -> Result<()> {
    let files = expand(&s.files)?;
    let raw = s.raw_dir();
    println!("== files ({} files → {})", files.len(), raw.display());
    if files.is_empty() {
        bail!("the inputs hold no .json or .ndjson files");
    }
    let mut types: BTreeMap<String, Resources> = BTreeMap::new();
    let mut skipped = 0;
    for file in &files {
        let mut batch = Vec::new();
        crate::views::load_file(file, &mut batch, s.threads)?;
        for r in batch {
            let Some(rtype) = r
                .get("resourceType")
                .and_then(Value::as_str)
                .filter(|t| crate::export::valid_type(t))
            else {
                skipped += 1;
                continue;
            };
            let t = types.entry(rtype.to_string()).or_default();
            let Some(id) = r.get("id").and_then(Value::as_str) else {
                t.list.push(r);
                continue;
            };
            match t.by_id.get(id) {
                Some(&i) => {
                    t.duplicates += 1;
                    if !is_older(&r, &t.list[i]) {
                        t.list[i] = r;
                    }
                }
                None => {
                    t.by_id.insert(id.to_string(), t.list.len());
                    t.list.push(r);
                }
            }
        }
    }
    if skipped > 0 {
        println!("   {skipped} entries without a valid resourceType skipped");
    }
    fs::create_dir_all(&raw).with_context(|| format!("creating {}", raw.display()))?;
    let export_state = raw.join(crate::export::STATE_FILE);
    if export_state.exists() {
        fs::remove_file(&export_state)
            .with_context(|| format!("removing {}", export_state.display()))?;
        println!("   removed the server export's state: the next server refresh downloads in full");
    }
    for (rtype, t) in &types {
        write_ndjson(&raw, rtype, &t.list)?;
        if t.duplicates > 0 {
            println!(
                "   {rtype}: {} resources ({} duplicates by id merged)",
                t.list.len(),
                t.duplicates
            );
        } else {
            println!("   {rtype}: {} resources", t.list.len());
        }
    }
    for stale in raw_types(&raw)? {
        if !types.contains_key(&stale) {
            let path = raw.join(format!("{stale}.ndjson"));
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            println!("   {stale}: no longer in the files; removed raw/{stale}.ndjson");
        }
    }
    Ok(())
}

/// Whether `candidate` is older than `current` by `meta.lastUpdated`. A
/// missing or unreadable value counts as older than any other; two missing
/// values are a tie.
fn is_older(candidate: &Value, current: &Value) -> bool {
    let updated = |r: &Value| {
        r.pointer("/meta/lastUpdated")
            .and_then(Value::as_str)
            .and_then(parse_instant)
    };
    updated(candidate) < updated(current)
}

/// `dir/<rtype>.ndjson`, through a hidden temporary file renamed into place, so
/// a failed write leaves the previous file.
fn write_ndjson(dir: &Path, rtype: &str, resources: &[Value]) -> Result<()> {
    let path = dir.join(format!("{rtype}.ndjson"));
    let tmp = dir.join(format!(".{rtype}.ndjson.tmp"));
    let result = (|| -> Result<()> {
        let mut out = BufWriter::new(fs::File::create(&tmp)?);
        for r in resources {
            serde_json::to_writer(&mut out, r)?;
            out.write_all(b"\n")?;
        }
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", tmp.display()));
    }
    fs::rename(&tmp, &path).with_context(|| format!("renaming {} into place", path.display()))
}

/// The types with a `<Type>.ndjson` in `dir`.
fn raw_types(dir: &Path) -> Result<Vec<String>> {
    let mut types = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if !path.is_file() || path.extension().is_none_or(|x| x != "ndjson") {
            continue;
        }
        if let Some(stem) = path.file_stem().map(|s| s.to_string_lossy())
            && crate::export::valid_type(&stem)
        {
            types.push(stem.into_owned());
        }
    }
    types.sort();
    Ok(types)
}

/// The locations step from files: raw/<name>/Location.ndjson (and
/// Organization.ndjson, if there is one) become the snapshot
/// raw/<name>/locations, with a state.json as an extract writes it, which the
/// transform turns into `out`/locations as after an extract. The state's
/// server is `files` and it has no watermark, so a server refresh afterwards
/// extracts in full (fhir.rs) instead of merging into this. Boundaries given
/// by URL are not fetched: the transform reports them as unresolved. Returns
/// false, having done nothing, when the files have no Locations.
pub fn locations(s: &Settings, out: &Path) -> Result<bool> {
    let raw = s.raw_dir();
    let source = raw.join("Location.ndjson");
    if !fs::metadata(&source).is_ok_and(|m| m.len() > 0) {
        println!("== locations: the files have no Location resources; skipped");
        return Ok(false);
    }
    let snap = Snapshot::new(&raw.join("locations"));
    println!(
        "== locations ({} → {} → {})",
        source.display(),
        snap.dir.display(),
        s.parquet_dir().join("locations").display()
    );
    fs::create_dir_all(&snap.dir).with_context(|| format!("creating {}", snap.dir.display()))?;
    let count = copy_into(&source, &snap.locations_tmp(), &snap.locations())?;
    let organizations = raw.join("Organization.ndjson");
    let organization_count = if organizations.is_file() {
        let n = copy_into(
            &organizations,
            &snap.organizations_tmp(),
            &snap.organizations(),
        )?;
        Some(n)
    } else {
        if snap.organizations().exists() {
            fs::remove_file(snap.organizations())
                .with_context(|| format!("removing {}", snap.organizations().display()))?;
        }
        None
    };
    State {
        server: FILES.into(),
        watermark: None,
        count,
        organization_watermark: None,
        organization_count,
        fhir_pipes_version: env!("CARGO_PKG_VERSION").to_string(),
        completed_at: format_utc(std::time::SystemTime::now()),
    }
    .write(&snap.state_path())?;
    println!(
        "   snapshot: {count} Locations{}",
        organization_count.map_or(String::new(), |n| format!(", {n} Organizations"))
    );
    crate::transform::run_transform(
        &TransformArgs {
            snapshot: snap.dir.clone(),
            out: out.to_path_buf(),
            country: None,
            row_group_size: DEFAULT_ROW_GROUP_SIZE,
            partition_by: DEFAULT_PARTITION_BY.to_string(),
            keep_without_geometry: false,
        },
        s.profile.as_deref(),
    )?;
    Ok(true)
}

/// Copy `from` to `to` through `tmp`. Returns the number of resources.
fn copy_into(from: &Path, tmp: &Path, to: &Path) -> Result<usize> {
    let text = fs::read_to_string(from).with_context(|| format!("reading {}", from.display()))?;
    let count = text.lines().filter(|l| !l.trim().is_empty()).count();
    fs::write(tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(tmp, to).with_context(|| format!("renaming {} into place", to.display()))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_later_last_updated_wins_and_missing_is_oldest() {
        let at = |t: &str| json!({"meta": {"lastUpdated": t}});
        assert!(is_older(
            &at("2026-01-01T00:00:00Z"),
            &at("2026-01-02T00:00:00Z")
        ));
        // Offsets are compared as instants.
        assert!(!is_older(
            &at("2026-01-01T10:00:00+02:00"),
            &at("2026-01-01T07:59:59Z")
        ));
        assert!(!is_older(&at("2026-01-01T00:00:00Z"), &json!({})));
        assert!(is_older(&json!({}), &at("2026-01-01T00:00:00Z")));
        // A tie is not older: the one read later is kept.
        assert!(!is_older(&json!({}), &json!({})));
    }

    #[test]
    fn inputs_expand_to_files_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::create_dir_all(d.join("sub/deeper")).unwrap();
        for f in [
            "b.ndjson",
            "a.json",
            "notes.txt",
            ".hidden.ndjson",
            "sub/deeper/c.json",
        ] {
            fs::write(d.join(f), "{}").unwrap();
        }
        let files = expand(&[d.join("sub"), d.join("*"), d.join("a.json")]).unwrap();
        let rel: Vec<String> = files
            .iter()
            .map(|f| f.strip_prefix(d).unwrap().display().to_string())
            .collect();
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            rel,
            [
                format!("sub{sep}deeper{sep}c.json"),
                "a.json".into(),
                "b.ndjson".into()
            ]
        );
        let err = expand(&[d.join("*.parquet")]).unwrap_err();
        assert!(err.to_string().contains("matches nothing"), "{err}");
        let err = expand(&[d.join("missing")]).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }
}
