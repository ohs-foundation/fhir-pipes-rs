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
use std::io::{BufReader, BufWriter, Read as _, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::cli::{DEFAULT_PARTITION_BY, DEFAULT_ROW_GROUP_SIZE, TransformArgs};
use crate::error::usage;
use crate::fhir::head::Head;
use crate::fs::AtomicFile;
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
            return Err(usage(anyhow::anyhow!(
                "input {} not found",
                input.display()
            )));
        }
        return crate::views::resource_files(input);
    }
    // Hidden files are skipped here as in a directory.
    let options = glob::MatchOptions {
        require_literal_leading_dot: true,
        ..Default::default()
    };
    let mut matches: Vec<PathBuf> = glob::glob_with(&text, options)
        .with_context(|| format!("input pattern {text}"))
        .map_err(usage)?
        .collect::<Result<_, _>>()
        .with_context(|| format!("input pattern {text}"))?;
    matches.sort();
    if matches.is_empty() {
        return Err(usage(anyhow::anyhow!(
            "input pattern {text} matches nothing"
        )));
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

/// Where a resource's text is: a line of an NDJSON input, or of the spill
/// file the resources of the other inputs are written to (`Sources`).
#[derive(Clone, Copy)]
struct Slot {
    source: usize,
    offset: u64,
    len: usize,
    /// Its `meta.lastUpdated`, when that is an instant.
    updated: Option<(i64, u32)>,
}

/// One type's resources, in the order first seen, one per id.
#[derive(Default)]
struct Resources {
    slots: Vec<Slot>,
    by_id: HashMap<String, usize>,
    duplicates: usize,
}

impl Resources {
    /// Add a resource. One whose id was seen before replaces it where it
    /// was, unless it is older.
    fn add(&mut self, id: Option<String>, slot: Slot) {
        let Some(id) = id else {
            self.slots.push(slot);
            return;
        };
        match self.by_id.get(&id) {
            Some(&i) => {
                self.duplicates += 1;
                if !is_older(slot.updated, self.slots[i].updated) {
                    self.slots[i] = slot;
                }
            }
            None => {
                self.by_id.insert(id, self.slots.len());
                self.slots.push(slot);
            }
        }
    }
}

/// Each type's resources, and the number without a valid type.
#[derive(Default)]
struct Index {
    types: BTreeMap<String, Resources>,
    skipped: usize,
}

impl Index {
    fn add(&mut self, rtype: Option<String>, id: Option<String>, slot: Slot) {
        match rtype.filter(|t| crate::export::valid_type(t)) {
            Some(t) => self.types.entry(t).or_default().add(id, slot),
            None => self.skipped += 1,
        }
    }

    /// Add `r`, writing it to the spill file.
    fn spill(&mut self, r: Value, sources: &mut Sources) -> Result<()> {
        let s = |p: &str| r.pointer(p).and_then(Value::as_str).map(str::to_string);
        let (rtype, id) = (s("/resourceType"), s("/id"));
        if !rtype.as_deref().is_some_and(crate::export::valid_type) {
            self.skipped += 1;
            return Ok(());
        }
        let updated = s("/meta/lastUpdated").as_deref().and_then(parse_instant);
        let (offset, len) = sources.spill(&r)?;
        let slot = Slot {
            source: 0,
            offset,
            len,
            updated,
        };
        self.add(rtype, id, slot);
        Ok(())
    }
}

/// What a line of an NDJSON input holds.
enum Read {
    /// One resource, of which only the head was read.
    One(Head),
    /// A Bundle's or an array's resources, or a value that is not an
    /// object.
    Many(Vec<Value>),
}

/// The files the resources' text is read back from: `sources[0]` is the
/// spill file, the rest the NDJSON inputs.
struct Sources {
    paths: Vec<PathBuf>,
    /// Each NDJSON input's length and modification time when it was first
    /// read, to tell whether it changed before it was read again.
    stamps: Vec<Option<(u64, std::time::SystemTime)>>,
    spill: BufWriter<fs::File>,
    spill_len: u64,
}

impl Sources {
    fn new(spill: &Path) -> Result<Self> {
        let file =
            fs::File::create(spill).with_context(|| format!("creating {}", spill.display()))?;
        Ok(Self {
            paths: vec![spill.to_path_buf()],
            stamps: vec![None],
            spill: BufWriter::new(file),
            spill_len: 0,
        })
    }

    fn stamp(path: &Path) -> Result<(u64, std::time::SystemTime)> {
        let m = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
        Ok((m.len(), m.modified()?))
    }

    /// Add an NDJSON input; returns its index.
    fn add_input(&mut self, path: &Path) -> Result<usize> {
        self.stamps.push(Some(Self::stamp(path)?));
        self.paths.push(path.to_path_buf());
        Ok(self.paths.len() - 1)
    }

    /// Write `r` to the spill file; returns where it is.
    fn spill(&mut self, r: &Value) -> Result<(u64, usize)> {
        let text = serde_json::to_vec(r)?;
        self.spill.write_all(&text)?;
        self.spill.write_all(b"\n")?;
        let at = self.spill_len;
        self.spill_len += text.len() as u64 + 1;
        Ok((at, text.len()))
    }

    /// Done writing: the spill file is flushed, to be read.
    fn finish_spill(&mut self) -> Result<()> {
        self.spill
            .flush()
            .with_context(|| format!("writing {}", self.paths[0].display()))
    }

    /// An error if an NDJSON input changed since it was first read.
    fn check_unchanged(&self) -> Result<()> {
        for (path, stamp) in self.paths.iter().zip(&self.stamps) {
            if let Some(stamp) = stamp
                && Self::stamp(path)? != *stamp
            {
                bail!("{} changed while it was read; run again", path.display());
            }
        }
        Ok(())
    }
}

/// Reads resources' text back from `Sources`, keeping the last file open:
/// a type's resources are mostly in one file, in order.
struct SlotReader<'a> {
    paths: &'a [PathBuf],
    open: Option<(usize, BufReader<fs::File>, u64)>,
}

impl SlotReader<'_> {
    fn read(&mut self, slot: &Slot, buf: &mut Vec<u8>) -> Result<()> {
        let path = &self.paths[slot.source];
        if self.open.as_ref().is_none_or(|(i, _, _)| *i != slot.source) {
            let f = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
            self.open = Some((slot.source, BufReader::with_capacity(1 << 20, f), 0));
        }
        let (_, reader, pos) = self.open.as_mut().expect("opened above");
        // Within the buffer this moves in it; beyond, it seeks.
        reader.seek_relative(slot.offset as i64 - *pos as i64)?;
        buf.resize(slot.len, 0);
        reader
            .read_exact(buf)
            .with_context(|| format!("reading {}", path.display()))?;
        *pos = slot.offset + slot.len as u64;
        Ok(())
    }
}

/// Removes the spill file however `split` ends.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Read every input file and write each resource type to
/// raw/<name>/<Type>.ndjson, written beside it and renamed into place. A
/// resource seen twice (same type and id) is kept once: the one with the later
/// `meta.lastUpdated`; on a tie, or when neither has one, the one read later.
/// A `<Type>.ndjson` left from an earlier run, of a type the files no longer
/// have, is removed, so the views do not read it.
///
/// The inputs are read twice, so they are never held whole. The first pass
/// keeps, for each type and id, the resource that wins and where its text
/// is: a line of an NDJSON input, read a chunk at a time and only far
/// enough to find its type, id and `meta.lastUpdated`, or, for a Bundle, an
/// array or a JSON file, its resources written to a spill file. The second
/// pass copies each type's kept resources' text, in the order first seen.
///
/// The directory can hold a server's download (`server.name` names both), so
/// its export state goes first: the watermarks in it describe the server's
/// files, not these, and a later server refresh must download in full.
pub fn split(s: &Settings) -> Result<()> {
    let files = expand(&s.files)?;
    let raw = s.raw_dir();
    println!("== files ({} files → {})", files.len(), raw.display());
    if files.is_empty() {
        return Err(usage(anyhow::anyhow!(
            "the inputs hold no .json or .ndjson files"
        )));
    }
    fs::create_dir_all(&raw).with_context(|| format!("creating {}", raw.display()))?;
    let spill_path = raw.join(".files-spill.ndjson.tmp");
    let _spill = RemoveOnDrop(spill_path.clone());
    let mut sources = Sources::new(&spill_path)?;
    let mut index = Index::default();
    for file in &files {
        if file.extension().is_none_or(|x| x != "ndjson") {
            let mut batch = Vec::new();
            crate::views::load_json_file(file, &mut batch)?;
            for r in batch {
                index.spill(r, &mut sources)?;
            }
            continue;
        }
        let source = sources.add_input(file)?;
        for chunk in crate::views::ndjson::open(file)? {
            let chunk = chunk?;
            let reads = chunk.parse(s.threads, |line| {
                Ok(match Head::parse(line)? {
                    Some(head) if head.resource_type.as_deref() != Some("Bundle") => {
                        Read::One(head)
                    }
                    _ => {
                        let mut out = Vec::new();
                        crate::views::push_resource(serde_json::from_str(line)?, &mut out);
                        Read::Many(out)
                    }
                })
            })?;
            for ((line, text), read) in chunk.lines().zip(reads) {
                match read {
                    Read::One(head) => index.add(
                        head.resource_type,
                        head.id,
                        Slot {
                            source,
                            offset: line.offset,
                            len: text.len(),
                            updated: head.last_updated.as_deref().and_then(parse_instant),
                        },
                    ),
                    Read::Many(resources) => {
                        for r in resources {
                            index.spill(r, &mut sources)?;
                        }
                    }
                }
            }
        }
    }
    sources.finish_spill()?;
    let Index { types, skipped } = index;
    if skipped > 0 {
        println!("   {skipped} entries without a valid resourceType skipped");
    }
    let export_state = raw.join(crate::export::STATE_FILE);
    if export_state.exists() {
        fs::remove_file(&export_state)
            .with_context(|| format!("removing {}", export_state.display()))?;
        println!("   removed the server export's state: the next server refresh downloads in full");
    }
    let mut reader = SlotReader {
        paths: &sources.paths,
        open: None,
    };
    // Every temporary file not yet renamed is removed if this returns early.
    let mut written = Vec::new();
    for (rtype, t) in &types {
        written.push(write_ndjson(&raw, rtype, &t.slots, &mut reader)?);
    }
    // A changed input could have put other text where a resource was.
    sources.check_unchanged()?;
    for ((rtype, t), file) in types.iter().zip(written) {
        let path = raw.join(format!("{rtype}.ndjson"));
        file.commit()
            .with_context(|| format!("renaming {} into place", path.display()))?;
        if t.duplicates > 0 {
            println!(
                "   {rtype}: {} resources ({} duplicates by id merged)",
                t.slots.len(),
                t.duplicates
            );
        } else {
            println!("   {rtype}: {} resources", t.slots.len());
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

/// Whether a resource last updated at `candidate` is older than one last
/// updated at `current`. A missing or unreadable value counts as older than
/// any other; two missing values are a tie.
fn is_older(candidate: Option<(i64, u32)>, current: Option<(i64, u32)>) -> bool {
    candidate < current
}

/// The resources at `slots` into `dir/.<rtype>.ndjson.tmp`, to be renamed
/// to `<rtype>.ndjson` once every type is written, so a failed write leaves
/// the previous file. Returns the temporary file, not yet committed.
fn write_ndjson(
    dir: &Path,
    rtype: &str,
    slots: &[Slot],
    reader: &mut SlotReader,
) -> Result<AtomicFile> {
    let path = dir.join(format!("{rtype}.ndjson"));
    let tmp = dir.join(format!(".{rtype}.ndjson.tmp"));
    let result = (|| -> Result<AtomicFile> {
        let mut out = BufWriter::new(AtomicFile::create_at(&path, tmp.clone())?);
        let mut buf = Vec::new();
        for slot in slots {
            reader.read(slot, &mut buf)?;
            out.write_all(&buf)?;
            out.write_all(b"\n")?;
        }
        Ok(out.into_inner().map_err(|e| e.into_error())?)
    })();
    result.with_context(|| format!("writing {}", tmp.display()))
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

/// Copy `from` to `to` through `tmp`. Returns the number of resources: its
/// non-blank lines, counted a chunk at a time.
fn copy_into(from: &Path, tmp: &Path, to: &Path) -> Result<usize> {
    let mut count = 0;
    for chunk in crate::views::ndjson::open(from)? {
        count += chunk?.len();
    }
    // Removes the copy if it fails part of the way.
    let file = AtomicFile::adopt(to, tmp.to_path_buf());
    fs::copy(from, tmp).with_context(|| format!("writing {}", tmp.display()))?;
    file.commit()
        .with_context(|| format!("renaming {} into place", to.display()))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_later_last_updated_wins_and_missing_is_oldest() {
        let at = |t: &str| parse_instant(t);
        assert!(is_older(
            at("2026-01-01T00:00:00Z"),
            at("2026-01-02T00:00:00Z")
        ));
        // Offsets are compared as instants.
        assert!(!is_older(
            at("2026-01-01T10:00:00+02:00"),
            at("2026-01-01T07:59:59Z")
        ));
        assert!(!is_older(at("2026-01-01T00:00:00Z"), None));
        assert!(is_older(None, at("2026-01-01T00:00:00Z")));
        // A tie is not older: the one read later is kept.
        assert!(!is_older(None, None));
    }

    /// What `split` wrote for each type, read back.
    fn split_types(raw: &Path) -> BTreeMap<String, Vec<Value>> {
        raw_types(raw)
            .unwrap()
            .into_iter()
            .map(|t| {
                let text = fs::read_to_string(raw.join(format!("{t}.ndjson"))).unwrap();
                let values = text
                    .lines()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect();
                (t, values)
            })
            .collect()
    }

    /// Every resource read whole, deduplicated as `split` does it: what it
    /// did before it read through an index.
    fn split_in_memory(files: &[PathBuf]) -> BTreeMap<String, Vec<Value>> {
        let mut all = Vec::new();
        for f in files {
            if f.extension().is_some_and(|x| x == "ndjson") {
                for line in fs::read_to_string(f).unwrap().lines() {
                    if !line.trim().is_empty() {
                        crate::views::push_resource(serde_json::from_str(line).unwrap(), &mut all);
                    }
                }
            } else {
                crate::views::load_json_file(f, &mut all).unwrap();
            }
        }
        let mut types: BTreeMap<String, (Vec<Value>, HashMap<String, usize>)> = BTreeMap::new();
        let updated = |r: &Value| {
            r.pointer("/meta/lastUpdated")
                .and_then(Value::as_str)
                .and_then(parse_instant)
        };
        for r in all {
            let Some(t) = r
                .get("resourceType")
                .and_then(Value::as_str)
                .filter(|t| crate::export::valid_type(t))
            else {
                continue;
            };
            let (list, by_id) = types.entry(t.to_string()).or_default();
            match r.get("id").and_then(Value::as_str) {
                None => list.push(r),
                Some(id) => match by_id.get(id) {
                    Some(&i) => {
                        if !is_older(updated(&r), updated(&list[i])) {
                            list[i] = r;
                        }
                    }
                    None => {
                        by_id.insert(id.to_string(), list.len());
                        list.push(r);
                    }
                },
            }
        }
        types.into_iter().map(|(t, (list, _))| (t, list)).collect()
    }

    #[test]
    fn split_keeps_what_reading_every_file_whole_kept() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        fs::create_dir_all(&input).unwrap();
        let obs = |id: &str, v: i64, at: Option<&str>| {
            let mut o = json!({"resourceType": "Observation", "id": id, "valueInteger": v});
            if let Some(at) = at {
                o["meta"] = json!({"lastUpdated": at});
            }
            o
        };
        let bundle = json!({"resourceType": "Bundle", "entry": [
            {"fullUrl": "urn:uuid:1", "resource": {"resourceType": "Patient", "id": "p1"}},
            {"resource": obs("o2", 20, Some("2026-02-01T00:00:00Z"))},
            {"resource": {"resourceType": "Observation", "subject": {"reference": "urn:uuid:1"}}},
        ]});
        fs::write(
            input.join("a.ndjson"),
            format!(
                "{}\r\n\r\n  {}  \n{}\n{}\n\"text\"\n{}\n{}\n",
                obs("o1", 1, Some("2026-03-01T00:00:00Z")),
                obs("o2", 2, Some("2026-01-01T00:00:00Z")),
                bundle,
                json!([obs("o3", 3, None), {"resourceType": "patient", "id": "bad"}]),
                json!({"resourceType": "Patient", "id": "p2", "n": 1.50}),
                obs("o1", 10, Some("2026-01-01T00:00:00+00:00")),
            ),
        )
        .unwrap();
        fs::write(
            input.join("b.json"),
            json!([obs("o3", 30, None), {"resourceType": "Patient", "id": "p2", "n": 2}])
                .to_string(),
        )
        .unwrap();
        fs::write(
            input.join("c.ndjson"),
            format!(
                "{}\n{}",
                obs("o2", 200, Some("2026-02-01T00:00:00Z")),
                json!({"resourceType": "Observation", "valueInteger": 7})
            ),
        )
        .unwrap();
        let o = super::super::config::Overrides {
            data: Some(dir.path().join("data")),
            input: vec![input.clone()],
            threads: Some(3),
            ..Default::default()
        };
        let file: super::super::config::File = toml::from_str("").unwrap();
        let s = super::super::config::settings(file, dir.path().to_path_buf(), o).unwrap();
        // A type the files no longer have is removed.
        fs::create_dir_all(s.raw_dir()).unwrap();
        fs::write(s.raw_dir().join("Task.ndjson"), "{}\n").unwrap();
        split(&s).unwrap();
        let got = split_types(&s.raw_dir());
        assert_eq!(got, split_in_memory(&expand(&s.files).unwrap()));
        let ids = |t: &str| -> Vec<Value> { got[t].iter().map(|r| r["id"].clone()).collect() };
        assert_eq!(
            ids("Observation"),
            [
                json!("o1"),
                json!("o2"),
                Value::Null,
                json!("o3"),
                Value::Null
            ]
        );
        assert_eq!(got["Observation"][1]["valueInteger"], 200);
        assert_eq!(got["Observation"][3]["valueInteger"], 30);
        assert_eq!(ids("Patient"), [json!("p1"), json!("p2")]);
        assert!(!s.raw_dir().join("Task.ndjson").exists());
        // A resource on a line of its own is copied as it is written there.
        let text = fs::read_to_string(s.raw_dir().join("Observation.ndjson")).unwrap();
        assert!(text.starts_with(&format!("{}\n", obs("o1", 1, Some("2026-03-01T00:00:00Z")))));
        let leftovers: Vec<_> = fs::read_dir(s.raw_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn copy_into_counts_the_resources() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from.ndjson");
        fs::write(&from, "{\"a\":1}\n\n  \n{\"b\":2}").unwrap();
        let (tmp, to) = (dir.path().join(".to.tmp"), dir.path().join("to.ndjson"));
        assert_eq!(copy_into(&from, &tmp, &to).unwrap(), 2);
        assert_eq!(fs::read(&to).unwrap(), fs::read(&from).unwrap());
        assert!(!tmp.exists());
    }

    #[test]
    fn copy_into_copies_a_read_only_source() {
        // A cached raw file the operator made read-only (mode 0444): the
        // copy keeps its permissions and is committed all the same.
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("Location.ndjson");
        fs::write(&from, "{\"a\":1}\n").unwrap();
        let read_only = |p: &Path, on: bool| {
            let mut perms = fs::metadata(p).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(on);
            fs::set_permissions(p, perms).unwrap();
        };
        read_only(&from, true);
        let (tmp, to) = (dir.path().join(".to.tmp"), dir.path().join("to.ndjson"));
        assert_eq!(copy_into(&from, &tmp, &to).unwrap(), 1);
        assert_eq!(fs::read(&to).unwrap(), fs::read(&from).unwrap());
        assert!(fs::metadata(&to).unwrap().permissions().readonly());
        assert!(!tmp.exists());
        // So the temporary directory can be removed on Windows.
        read_only(&from, false);
        read_only(&to, false);
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
