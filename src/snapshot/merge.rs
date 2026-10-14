//! Phase three: stream the old snapshot and the incoming file into a new
//! locations.ndjson, inlining boundary bytes and computing the watermark.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use base64::Engine;
use serde_json::Value;

use crate::error::{PipesError, Result};
use crate::extract::page::PageNote;
use crate::fhir::location::{BOUNDARY_EXTENSION_URLS, GEOJSON_CONTENT_TYPE};
use crate::fhir::ndjson::NdjsonReader;
use crate::report::Report;
use crate::snapshot::instant::later;
use crate::snapshot::Snapshot;

#[derive(Debug)]
pub struct MergeStats {
    pub total: usize,
    pub added: usize,
    pub updated: usize,
    pub watermark: Option<String>,
}

/// The parts of a merge that stay fixed while it runs, bundled so the
/// worker function doesn't need a long argument list.
struct MergeInput<'a> {
    notes: &'a [PageNote],
    full: bool,
    lookup: &'a dyn Fn(&str) -> Option<Vec<u8>>,
    failures: &'a HashMap<String, String>,
}

/// Replace the boundary attachment that carries `url` with inline base64 data.
fn inline_boundary(resource: &mut Value, url: &str, bytes: &[u8]) -> bool {
    let Some(exts) = resource.get_mut("extension").and_then(Value::as_array_mut) else {
        return false;
    };
    for ext in exts.iter_mut() {
        let is_boundary = ext
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(|u| BOUNDARY_EXTENSION_URLS.contains(&u));
        if !is_boundary {
            continue;
        }
        let Some(att) = ext
            .get_mut("valueAttachment")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        if att.get("url").and_then(Value::as_str) != Some(url) {
            continue;
        }
        att.remove("url");
        att.insert(
            "data".into(),
            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
        );
        att.entry("contentType")
            .or_insert_with(|| Value::String(GEOJSON_CONTENT_TYPE.into()));
        return true;
    }
    false
}

/// Just enough of a Location to upsert by id and compare watermarks -- a
/// small typed struct instead of a full `Value` parse, measured 3.6x faster.
#[derive(serde::Deserialize)]
struct Head {
    id: Option<String>,
    meta: Option<Meta>,
}

#[derive(serde::Deserialize)]
struct Meta {
    #[serde(rename = "lastUpdated")]
    last_updated: Option<String>,
}

fn id_and_updated(text: &str) -> Option<(String, Option<String>)> {
    let head: Head = serde_json::from_str(text).ok()?;
    let id = head.id?;
    let updated = head.meta.and_then(|m| m.last_updated);
    Some((id, updated))
}

/// Stream `locations.ndjson` and `.incoming.ndjson` into a new
/// `locations.ndjson`, upserting by id.
///
/// A line surviving from the old snapshot is any old id not present among
/// `notes`; every incoming line replaces its old counterpart (if any). When
/// a note carries a `boundary_url`, `lookup` is tried for the raw boundary
/// bytes to inline; a lookup miss is reported via `failures` (the reason a
/// fetch did not happen) rather than failing the merge. `full` skips the old
/// snapshot entirely, so a `--full` re-extract starts clean. An old line
/// whose id can't be parsed is copied through with an unknown id, so it may
/// coexist with an incoming line that happens to share the id it can't see.
/// On success the new file is renamed over `locations.ndjson` and the
/// incoming file is removed; on failure the temp file is cleaned up and
/// neither is touched.
pub fn merge(
    snap: &Snapshot,
    notes: &[PageNote],
    full: bool,
    lookup: &dyn Fn(&str) -> Option<Vec<u8>>,
    failures: &HashMap<String, String>,
    report: &mut Report,
) -> Result<MergeStats> {
    merge_file(
        &MergeFiles::locations(snap),
        notes,
        full,
        lookup,
        failures,
        report,
    )
}

/// The three files one merge touches, plus the report kind for a line in
/// the existing file that cannot be parsed.
pub struct MergeFiles {
    pub existing: std::path::PathBuf,
    pub tmp: std::path::PathBuf,
    pub incoming: std::path::PathBuf,
    pub unparsed_kind: &'static str,
}

impl MergeFiles {
    pub fn locations(snap: &Snapshot) -> Self {
        Self {
            existing: snap.locations(),
            tmp: snap.locations_tmp(),
            incoming: snap.incoming(),
            unparsed_kind: "snapshot_line_unparsed",
        }
    }
    pub fn organizations(snap: &Snapshot) -> Self {
        Self {
            existing: snap.organizations(),
            tmp: snap.organizations_tmp(),
            incoming: snap.incoming_organizations(),
            unparsed_kind: "organization_line_unparsed",
        }
    }
}

/// `merge` for any snapshot file. Boundary inlining only ever triggers on
/// notes that carry a `boundary_url`, which Organizations never do.
pub fn merge_file(
    files: &MergeFiles,
    notes: &[PageNote],
    full: bool,
    lookup: &dyn Fn(&str) -> Option<Vec<u8>>,
    failures: &HashMap<String, String>,
    report: &mut Report,
) -> Result<MergeStats> {
    let input = MergeInput {
        notes,
        full,
        lookup,
        failures,
    };
    let result = merge_into(files, &input, report);
    if result.is_err() {
        let _ = std::fs::remove_file(&files.tmp);
    }
    result
}

fn merge_into(files: &MergeFiles, input: &MergeInput, report: &mut Report) -> Result<MergeStats> {
    let tmp: &Path = &files.tmp;
    let incoming_ids: HashSet<&str> = input.notes.iter().map(|n| n.id.as_str()).collect();
    // Last occurrence of each id in the incoming file wins.
    let mut last_index: HashMap<&str, usize> = HashMap::new();
    for (i, n) in input.notes.iter().enumerate() {
        last_index.insert(&n.id, i);
    }

    let file = std::fs::File::create(tmp).map_err(|e| PipesError::io(tmp, e))?;
    let mut out = std::io::BufWriter::new(file);
    let mut stats = MergeStats {
        total: 0,
        added: 0,
        updated: 0,
        watermark: None,
    };
    let mut old_ids: HashSet<String> = HashSet::new();

    if !input.full && files.existing.exists() {
        for line in NdjsonReader::open(&files.existing)? {
            let line = line?;
            match id_and_updated(&line.text) {
                Some((id, _)) if incoming_ids.contains(id.as_str()) => {
                    // Recorded even though this line itself is dropped: the
                    // incoming line that replaces it below checks `old_ids`
                    // to tell an update from a new row.
                    old_ids.insert(id);
                    continue;
                }
                Some((id, updated)) => {
                    if let Some(u) = updated {
                        stats.watermark = later(stats.watermark.as_deref(), &u);
                    }
                    old_ids.insert(id);
                }
                None => report.add(
                    files.unparsed_kind,
                    &format!("line {}", line.number),
                    "not a JSON object with an id; copied unchanged",
                ),
            }
            writeln!(out, "{}", line.text).map_err(|e| PipesError::io(tmp, e))?;
            stats.total += 1;
        }
    }

    let mut seen = 0usize;
    for (i, line) in NdjsonReader::open(&files.incoming)?.enumerate() {
        let line = line?;
        seen += 1;
        let Some(note) = input.notes.get(i) else {
            return Err(PipesError::Environment(
                "incoming file and page notes disagree; rerun extract".into(),
            ));
        };
        if last_index.get(note.id.as_str()) != Some(&i) {
            continue;
        }
        match &note.boundary_url {
            // No boundary to inline: the line is already exactly what
            // belongs in the new snapshot (preserve_order is on and the
            // pager wrote compact JSON), so copy it through unparsed
            // instead of paying for a parse + re-serialize.
            None => {
                writeln!(out, "{}", line.text).map_err(|e| PipesError::io(tmp, e))?;
            }
            Some(url) => {
                let mut resource: Value = serde_json::from_str(&line.text)?;
                match (input.lookup)(url) {
                    Some(bytes) => {
                        inline_boundary(&mut resource, url, &bytes);
                    }
                    None => report.add(
                        "boundary_fetch_failed",
                        &note.id,
                        &format!(
                            "{url}: {}",
                            input
                                .failures
                                .get(url.as_str())
                                .map(String::as_str)
                                .unwrap_or("not fetched")
                        ),
                    ),
                }
                serde_json::to_writer(&mut out, &resource).map_err(|e| {
                    match e.io_error_kind() {
                        Some(kind) => PipesError::io(tmp, std::io::Error::from(kind)),
                        None => PipesError::Json(e),
                    }
                })?;
                out.write_all(b"\n").map_err(|e| PipesError::io(tmp, e))?;
            }
        }
        if let Some(u) = &note.last_updated {
            stats.watermark = later(stats.watermark.as_deref(), u);
        }
        stats.total += 1;
        if old_ids.contains(&note.id) {
            stats.updated += 1;
        } else {
            stats.added += 1;
        }
    }
    if seen != input.notes.len() {
        return Err(PipesError::Environment(
            "incoming file and page notes disagree; rerun extract".into(),
        ));
    }
    out.flush().map_err(|e| PipesError::io(tmp, e))?;
    let file = out
        .into_inner()
        .map_err(|e| PipesError::io(tmp, e.into_error()))?;
    file.sync_all().map_err(|e| PipesError::io(tmp, e))?;
    std::fs::rename(tmp, &files.existing).map_err(|e| PipesError::io(tmp, e))?;
    // Best-effort, as in `State::write`: not every platform/filesystem
    // supports fsync on a directory, and that's not worth failing an
    // otherwise-successful merge over.
    if let Some(dir) = files.existing.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    let _ = std::fs::remove_file(&files.incoming);
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::page::PageNote;

    #[test]
    fn merge_file_upserts_organizations_without_touching_locations() {
        let dir = tempfile::tempdir().unwrap();
        let snap = Snapshot::new(dir.path());
        std::fs::write(snap.locations(), "{\"id\":\"loc\"}\n").unwrap();
        std::fs::write(
            snap.organizations(),
            "{\"resourceType\":\"Organization\",\"id\":\"org-a\",\"meta\":{\"lastUpdated\":\"2026-01-01T00:00:00Z\"},\"name\":\"old\"}\n",
        )
        .unwrap();
        std::fs::write(
            snap.incoming_organizations(),
            "{\"resourceType\":\"Organization\",\"id\":\"org-a\",\"meta\":{\"lastUpdated\":\"2026-01-02T00:00:00Z\"},\"name\":\"new\"}\n{\"resourceType\":\"Organization\",\"id\":\"org-b\",\"meta\":{\"lastUpdated\":\"2026-01-03T00:00:00Z\"}}\n",
        )
        .unwrap();
        let notes = vec![
            PageNote {
                id: "org-a".into(),
                last_updated: Some("2026-01-02T00:00:00Z".into()),
                boundary_url: None,
            },
            PageNote {
                id: "org-b".into(),
                last_updated: Some("2026-01-03T00:00:00Z".into()),
                boundary_url: None,
            },
        ];
        let mut report = Report::default();
        let files = MergeFiles::organizations(&snap);
        let stats = merge_file(
            &files,
            &notes,
            false,
            &|_| None,
            &HashMap::new(),
            &mut report,
        )
        .unwrap();
        assert_eq!((stats.total, stats.added, stats.updated), (2, 1, 1));
        assert_eq!(stats.watermark.as_deref(), Some("2026-01-03T00:00:00Z"));
        let text = std::fs::read_to_string(snap.organizations()).unwrap();
        assert!(text.contains("\"name\":\"new\""));
        assert!(!text.contains("old"));
        assert_eq!(
            std::fs::read_to_string(snap.locations()).unwrap(),
            "{\"id\":\"loc\"}\n"
        );
        assert!(!snap.incoming_organizations().exists());
        assert!(!snap.organizations_tmp().exists());
    }
    use crate::report::Report;
    use std::collections::HashMap;

    fn line(id: &str, updated: &str, boundary_url: Option<&str>) -> String {
        let mut r =
            serde_json::json!({"resourceType":"Location","id":id,"meta":{"lastUpdated":updated}});
        if let Some(u) = boundary_url {
            r["extension"] = serde_json::json!([{"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson","valueAttachment":{"url":u}}]);
        }
        r.to_string()
    }
    fn note(id: &str, updated: &str, url: Option<&str>) -> PageNote {
        PageNote {
            id: id.into(),
            last_updated: Some(updated.into()),
            boundary_url: url.map(str::to_string),
        }
    }
    fn ids(text: &str) -> Vec<String> {
        text.lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .map(|v| v["id"].as_str().unwrap_or("?").to_string())
                    .unwrap_or_else(|_| "<unparsed>".into())
            })
            .collect()
    }

    #[test]
    fn upserts_inlines_and_advances_the_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let snap = Snapshot::new(dir.path());
        std::fs::write(
            snap.locations(),
            format!(
                "{}\n{}\n",
                line("a", "2026-01-01T00:00:00Z", None),
                line("b", "2026-01-02T00:00:00Z", None)
            ),
        )
        .unwrap();
        std::fs::write(
            snap.incoming(),
            format!(
                "{}\n{}\n",
                line("b", "2026-01-03T00:00:00Z", Some("https://x/b")),
                line("c", "2026-01-04T00:00:00Z", None)
            ),
        )
        .unwrap();
        let notes = vec![
            note("b", "2026-01-03T00:00:00Z", Some("https://x/b")),
            note("c", "2026-01-04T00:00:00Z", None),
        ];
        let mut bytes = HashMap::new();
        bytes.insert(
            "https://x/b".to_string(),
            b"{\"type\":\"Point\",\"coordinates\":[1,2]}".to_vec(),
        );
        let lookup = |u: &str| bytes.get(u).cloned();
        let mut report = Report::default();
        let stats = merge(&snap, &notes, false, &lookup, &HashMap::new(), &mut report).unwrap();
        assert_eq!((stats.total, stats.added, stats.updated), (3, 1, 1));
        assert_eq!(stats.watermark.as_deref(), Some("2026-01-04T00:00:00Z"));
        let text = std::fs::read_to_string(snap.locations()).unwrap();
        assert_eq!(ids(&text), vec!["a", "b", "c"]);
        let b: serde_json::Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
        let att = &b["extension"][0]["valueAttachment"];
        assert!(att.get("url").is_none());
        assert_eq!(att["contentType"], "application/geo+json");
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            att["data"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(decoded, b"{\"type\":\"Point\",\"coordinates\":[1,2]}");
        assert!(!snap.locations_tmp().exists());
        assert!(!snap.incoming().exists());
        assert_eq!(report.counts().len(), 0);
    }

    #[test]
    fn full_ignores_the_old_snapshot_and_a_failed_boundary_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let snap = Snapshot::new(dir.path());
        std::fs::write(
            snap.locations(),
            format!("{}\n", line("old", "2026-01-01T00:00:00Z", None)),
        )
        .unwrap();
        std::fs::write(
            snap.incoming(),
            format!(
                "{}\n",
                line("n", "2026-01-02T00:00:00Z", Some("https://x/n"))
            ),
        )
        .unwrap();
        let notes = vec![note("n", "2026-01-02T00:00:00Z", Some("https://x/n"))];
        let mut failures = HashMap::new();
        failures.insert("https://x/n".to_string(), "HTTP 404".to_string());
        let mut report = Report::default();
        let stats = merge(&snap, &notes, true, &|_| None, &failures, &mut report).unwrap();
        assert_eq!((stats.total, stats.added, stats.updated), (1, 1, 0));
        assert_eq!(report.count("boundary_fetch_failed"), 1);
        let text = std::fs::read_to_string(snap.locations()).unwrap();
        assert!(!text.contains("\"old\""));
        assert!(text.contains("https://x/n"), "attachment left as a url");
    }

    #[test]
    fn duplicate_incoming_ids_keep_the_last_and_unparsed_old_lines_are_copied() {
        let dir = tempfile::tempdir().unwrap();
        let snap = Snapshot::new(dir.path());
        std::fs::write(snap.locations(), "not json\n").unwrap();
        std::fs::write(
            snap.incoming(),
            format!(
                "{}\n{}\n",
                line("d", "2026-01-01T00:00:00Z", None),
                line("d", "2026-01-05T00:00:00Z", None)
            ),
        )
        .unwrap();
        let notes = vec![
            note("d", "2026-01-01T00:00:00Z", None),
            note("d", "2026-01-05T00:00:00Z", None),
        ];
        let mut report = Report::default();
        let stats = merge(
            &snap,
            &notes,
            false,
            &|_| None,
            &HashMap::new(),
            &mut report,
        )
        .unwrap();
        assert_eq!(stats.total, 2, "the unparsed line plus d");
        assert_eq!((stats.added, stats.updated), (1, 0));
        assert_eq!(report.count("snapshot_line_unparsed"), 1);
        let text = std::fs::read_to_string(snap.locations()).unwrap();
        assert_eq!(text.matches("\"id\":\"d\"").count(), 1);
        assert!(text.contains("2026-01-05"));
        assert_eq!(stats.watermark.as_deref(), Some("2026-01-05T00:00:00Z"));
    }

    #[test]
    fn no_existing_snapshot_is_fine_and_old_watermark_counts() {
        let dir = tempfile::tempdir().unwrap();
        let snap = Snapshot::new(dir.path());
        std::fs::write(
            snap.incoming(),
            format!("{}\n", line("a", "2026-01-01T00:00:00Z", None)),
        )
        .unwrap();
        let notes = vec![note("a", "2026-01-01T00:00:00Z", None)];
        let stats = merge(
            &snap,
            &notes,
            false,
            &|_| None,
            &HashMap::new(),
            &mut Report::default(),
        )
        .unwrap();
        assert_eq!((stats.total, stats.added), (1, 1));
        // A second incremental run with an older incoming line keeps the newer old watermark.
        std::fs::write(
            snap.incoming(),
            format!("{}\n", line("z", "2025-01-01T00:00:00Z", None)),
        )
        .unwrap();
        let notes = vec![note("z", "2025-01-01T00:00:00Z", None)];
        let stats = merge(
            &snap,
            &notes,
            false,
            &|_| None,
            &HashMap::new(),
            &mut Report::default(),
        )
        .unwrap();
        assert_eq!(stats.watermark.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(stats.total, 2);
    }

    #[test]
    fn a_note_count_mismatch_is_an_environment_error() {
        let dir = tempfile::tempdir().unwrap();
        let snap = Snapshot::new(dir.path());
        std::fs::write(
            snap.incoming(),
            format!(
                "{}\n{}\n",
                line("a", "2026-01-01T00:00:00Z", None),
                line("b", "2026-01-01T00:00:00Z", None)
            ),
        )
        .unwrap();
        let notes = vec![note("a", "2026-01-01T00:00:00Z", None)];
        let err = merge(
            &snap,
            &notes,
            true,
            &|_| None,
            &HashMap::new(),
            &mut Report::default(),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(!snap.locations().exists(), "nothing renamed into place");
        assert!(!snap.locations_tmp().exists(), "tmp cleaned up");
    }

    #[test]
    fn two_boundary_extensions_only_the_url_one_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let snap = Snapshot::new(dir.path());
        let src = r#"{"resourceType":"Location","id":"m","meta":{"lastUpdated":"2026-01-01T00:00:00Z"},"extension":[{"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson","valueAttachment":{"contentType":"application/geo+json","data":"already"}},{"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson","valueAttachment":{"url":"https://x/m"}}]}"#;
        std::fs::write(snap.incoming(), format!("{src}\n")).unwrap();
        let notes = vec![note("m", "2026-01-01T00:00:00Z", Some("https://x/m"))];
        let mut bytes = HashMap::new();
        bytes.insert("https://x/m".to_string(), b"new bytes".to_vec());
        let lookup = |u: &str| bytes.get(u).cloned();
        let mut report = Report::default();
        merge(&snap, &notes, false, &lookup, &HashMap::new(), &mut report).unwrap();
        let text = std::fs::read_to_string(snap.locations()).unwrap();
        let v: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        let exts = v["extension"].as_array().unwrap();
        assert_eq!(exts[0]["valueAttachment"]["data"], "already", "untouched");
        assert!(exts[1]["valueAttachment"].get("url").is_none());
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            exts[1]["valueAttachment"]["data"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(decoded, b"new bytes");
    }

    #[test]
    fn hl7_boundary_url_is_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let snap = Snapshot::new(dir.path());
        let src = r#"{"resourceType":"Location","id":"h","meta":{"lastUpdated":"2026-01-01T00:00:00Z"},"extension":[{"url":"http://hl7.org/fhir/StructureDefinition/location-boundary-geojson","valueAttachment":{"url":"https://x/h"}}]}"#;
        std::fs::write(snap.incoming(), format!("{src}\n")).unwrap();
        let notes = vec![note("h", "2026-01-01T00:00:00Z", Some("https://x/h"))];
        let mut bytes = HashMap::new();
        bytes.insert("https://x/h".to_string(), b"hl7 bytes".to_vec());
        let lookup = |u: &str| bytes.get(u).cloned();
        let mut report = Report::default();
        merge(&snap, &notes, false, &lookup, &HashMap::new(), &mut report).unwrap();
        let text = std::fs::read_to_string(snap.locations()).unwrap();
        let v: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        let att = &v["extension"][0]["valueAttachment"];
        assert!(att.get("url").is_none());
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            att["data"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(decoded, b"hl7 bytes");
    }

    #[test]
    fn a_large_line_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let snap = Snapshot::new(dir.path());
        let padding = "x".repeat(4 * 1024 * 1024);
        let mut resource = serde_json::json!({
            "resourceType":"Location","id":"big","meta":{"lastUpdated":"2026-01-01T00:00:00Z"},
        });
        resource["padding"] = serde_json::Value::String(padding);
        let src = resource.to_string();
        std::fs::write(snap.incoming(), format!("{src}\n")).unwrap();
        let notes = vec![note("big", "2026-01-01T00:00:00Z", None)];
        let stats = merge(
            &snap,
            &notes,
            false,
            &|_| None,
            &HashMap::new(),
            &mut Report::default(),
        )
        .unwrap();
        assert_eq!((stats.total, stats.added), (1, 1));
        let text = std::fs::read_to_string(snap.locations()).unwrap();
        assert_eq!(text, format!("{src}\n"), "byte-identical raw copy");
    }

    #[test]
    fn an_id_in_old_and_twice_in_incoming_counts_as_one_update() {
        let dir = tempfile::tempdir().unwrap();
        let snap = Snapshot::new(dir.path());
        std::fs::write(
            snap.locations(),
            format!("{}\n", line("x", "2025-01-01T00:00:00Z", None)),
        )
        .unwrap();
        std::fs::write(
            snap.incoming(),
            format!(
                "{}\n{}\n",
                line("x", "2026-01-02T00:00:00Z", None),
                line("x", "2026-01-03T00:00:00Z", None)
            ),
        )
        .unwrap();
        let notes = vec![
            note("x", "2026-01-02T00:00:00Z", None),
            note("x", "2026-01-03T00:00:00Z", None),
        ];
        let stats = merge(
            &snap,
            &notes,
            false,
            &|_| None,
            &HashMap::new(),
            &mut Report::default(),
        )
        .unwrap();
        assert_eq!((stats.total, stats.added, stats.updated), (1, 0, 1));
        assert_eq!(stats.watermark.as_deref(), Some("2026-01-03T00:00:00Z"));
    }
}
