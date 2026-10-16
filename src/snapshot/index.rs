//! `id -> (offset, len)` over one NDJSON file. Nothing else is retained; a
//! resource is re-read only when something names it.

use std::collections::HashMap;
use std::path::Path;

use crate::error::Result;
use crate::fhir::head::Head;
use crate::fhir::ndjson::NdjsonReader;
use crate::report::Report;

/// Index one NDJSON file by resource id. A repeated id keeps the first
/// line and is reported as `duplicate_id`; a line that is not an object
/// with a string id is reported under `unparsed_kind` and skipped.
pub fn index_by_id(
    path: &Path,
    report: &mut Report,
    unparsed_kind: &str,
) -> Result<HashMap<String, (u64, usize)>> {
    let mut index = HashMap::new();
    for line in NdjsonReader::open(path)? {
        let line = line?;
        let id = Head::parse(&line.text).ok().flatten().and_then(|h| h.id);
        match id {
            Some(id) => match index.entry(id) {
                std::collections::hash_map::Entry::Occupied(e) => report.add(
                    "duplicate_id",
                    e.key(),
                    &format!("line {} repeats an earlier id; first kept", line.number),
                ),
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((line.offset, line.len));
                }
            },
            None => report.add(
                unparsed_kind,
                "<unknown>",
                &format!("line {} is not a resource with an id", line.number),
            ),
        }
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn indexes_ids_keeps_the_first_duplicate_and_reports_unparsed_lines() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"id":"a","n":1}}"#).unwrap();
        writeln!(f, "not json").unwrap();
        writeln!(f, r#"{{"id":"a","n":2}}"#).unwrap();
        writeln!(f, r#"{{"id":"b"}}"#).unwrap();
        // Repeated field names: the last value, as a `Value` read gives.
        writeln!(f, r#"{{"resourceType":"X","resourceType":"X","id":"c"}}"#).unwrap();
        writeln!(f, r#"{{"id":"x","id":"d"}}"#).unwrap();
        let mut report = Report::default();
        let index = index_by_id(f.path(), &mut report, "organization_line_unparsed").unwrap();
        assert_eq!(index.len(), 4);
        assert!(index.contains_key("c") && index.contains_key("d"));
        assert!(!index.contains_key("x"));
        assert_eq!(index["a"], (0, 16));
        assert_eq!(report.count("duplicate_id"), 1);
        assert_eq!(report.count("organization_line_unparsed"), 1);
    }
}
