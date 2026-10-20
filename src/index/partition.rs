//! Which columns a row can be partitioned by, and how a value becomes a
//! filesystem-safe, collision-free `key=value` directory segment.

use std::collections::{HashMap, HashSet};

use sha2::{Digest, Sha256};

use crate::error::{PipesError, Result};
use crate::index::IndexRecord;
use crate::report::Report;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionKey {
    Country,
    GeomType,
    Tier,
    Type,
}

impl PartitionKey {
    pub fn name(self) -> &'static str {
        match self {
            PartitionKey::Country => "country",
            PartitionKey::GeomType => "geom_type",
            PartitionKey::Tier => "tier",
            PartitionKey::Type => "type",
        }
    }

    /// The row's value for this key, rendered as text. `"null"` for a missing value.
    pub fn value(self, rec: &IndexRecord) -> &str {
        match self {
            PartitionKey::Country => &rec.country,
            PartitionKey::GeomType => rec.geometry.map(|g| g.kind.as_str()).unwrap_or("null"),
            PartitionKey::Tier => &rec.tier,
            PartitionKey::Type => rec.type_code.as_deref().unwrap_or("null"),
        }
    }
}

/// Parse a comma-separated `--partition-by` spec. Empty fragments from a
/// trailing comma are skipped; an entirely empty spec, an unknown key, and a
/// repeated key are all reported as usage errors.
pub fn parse_keys(spec: &str) -> Result<Vec<PartitionKey>> {
    let mut keys = Vec::new();
    let mut seen = HashSet::new();
    for raw in spec.split(',') {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = match trimmed {
            "country" => PartitionKey::Country,
            "geom_type" => PartitionKey::GeomType,
            "tier" => PartitionKey::Tier,
            "type" => PartitionKey::Type,
            other => {
                return Err(PipesError::Usage(format!(
                    "--partition-by: unknown key {other:?}; available: country, geom_type, tier, type"
                )));
            }
        };
        if !seen.insert(trimmed) {
            return Err(PipesError::Usage(format!(
                "--partition-by: key {trimmed} repeated"
            )));
        }
        keys.push(key);
    }
    if keys.is_empty() {
        return Err(PipesError::Usage(
            "--partition-by: no keys given".to_string(),
        ));
    }
    Ok(keys)
}

const RESERVED_WINDOWS_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Render one partition value as filesystem-safe text, unconditionally
/// sanitising for the strictest common target (a dataset written on Linux
/// must survive copying to Windows or a case-insensitive macOS volume).
///
/// Returns `(candidate, was_sanitized)`. NUL is dropped outright; `/`, `\`,
/// the other characters Windows forbids in a filename, and every remaining
/// control character become `_`; trailing dots and spaces (which Windows
/// silently strips) are trimmed; a result that collides with a reserved
/// Windows device name (case-insensitively, looking only at the portion
/// before the first `.` -- Windows reserves `CON.txt` exactly as it
/// reserves bare `CON`) gets a `_` appended to that base portion, e.g.
/// `CON.txt` -> `CON_.txt`. An empty result becomes `"empty"`.
fn sanitize(value: &str) -> (String, bool) {
    let mut result: String = value
        .chars()
        .filter(|&c| c != '\0')
        .map(|c| match c {
            '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();

    while matches!(result.chars().last(), Some('.') | Some(' ')) {
        result.pop();
    }

    let base_end = result.find('.').unwrap_or(result.len());
    if RESERVED_WINDOWS_NAMES
        .iter()
        .any(|name| name.eq_ignore_ascii_case(&result[..base_end]))
    {
        result.insert(base_end, '_');
    }

    if result.is_empty() {
        result = "empty".to_string();
    }

    // A file name is at most 255 bytes on most filesystems, and `key=` and
    // a collision suffix come on top: a longer value is cut, and a hash of
    // the whole value keeps two values that start the same apart.
    if result.len() > MAX_SEGMENT {
        let mut cut = MAX_SEGMENT - 9;
        while !result.is_char_boundary(cut) {
            cut -= 1;
        }
        let digest = Sha256::digest(value.as_bytes());
        result = format!(
            "{}~{:02x}{:02x}{:02x}{:02x}",
            &result[..cut],
            digest[0],
            digest[1],
            digest[2],
            digest[3]
        );
    }

    let sanitized = result != value;
    (result, sanitized)
}

/// The longest partition value written as it is, in bytes.
const MAX_SEGMENT: usize = 200;

/// Segment claims within one parent directory, keyed case-insensitively:
/// lowercased segment -> (original-case segment, owning value). A
/// case-insensitive filesystem (Windows, default macOS) would otherwise
/// merge two directories that differ only in case with no report.
#[derive(Debug, Default)]
pub struct Claims(HashMap<String, (String, String)>);

/// Render one `--partition-by` value as a filesystem-safe, collision-free
/// directory name segment (without the `key=` prefix), scoped to `claims`
/// (one `Claims` per parent-directory-and-key context).
///
/// Two distinct values that render (or sanitise) to the same segment --
/// case-insensitively -- must not silently share a directory: the second
/// one to claim a segment gets a short deterministic hash of its own
/// candidate and value appended, so the two never collide on disk. A value
/// that is both sanitised and collides only reports the collision.
pub fn segment(key: &str, value: &str, claims: &mut Claims, report: &mut Report) -> String {
    let (candidate, sanitized) = sanitize(value);
    let lower = candidate.to_lowercase();

    match claims.0.get(&lower) {
        None => {
            claims
                .0
                .insert(lower, (candidate.clone(), value.to_string()));
            if sanitized {
                report.add(
                    "partition_value_sanitized",
                    &format!("{key}={value}"),
                    &format!(
                        "cannot be a directory name as it is (a path separator, a control character, a reserved name or more than {MAX_SEGMENT} bytes); written to the directory {key}={candidate:?} instead"
                    ),
                );
            }
            candidate
        }
        Some((existing_segment, owner)) if owner == value => existing_segment.clone(),
        Some(_) => {
            let mut salt = String::new();
            loop {
                let digest = Sha256::digest(format!("{candidate}\0{value}{salt}").as_bytes());
                let seg = format!(
                    "{candidate}~{:02x}{:02x}{:02x}{:02x}",
                    digest[0], digest[1], digest[2], digest[3]
                );
                let seg_lower = seg.to_lowercase();
                match claims.0.get(&seg_lower) {
                    Some((_, owner)) if owner != value => salt.push('#'),
                    _ => {
                        claims.0.insert(seg_lower, (seg.clone(), value.to_string()));
                        report.add(
                            "partition_value_collision",
                            &format!("{key}={value}"),
                            &format!(
                                "renders to the same directory segment {key}={candidate:?} as a different value already written under this partition; disambiguated to {key}={seg:?} instead"
                            ),
                        );
                        return seg;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Report;

    #[test]
    fn parses_and_validates_keys() {
        assert_eq!(
            parse_keys("country,geom_type").unwrap(),
            vec![PartitionKey::Country, PartitionKey::GeomType]
        );
        assert!(
            parse_keys("country,nope")
                .unwrap_err()
                .to_string()
                .contains("nope")
        );
    }

    #[test]
    fn parse_keys_rejects_repeats_and_empty() {
        assert!(
            parse_keys("")
                .unwrap_err()
                .to_string()
                .contains("no keys given")
        );
        assert!(
            parse_keys(",")
                .unwrap_err()
                .to_string()
                .contains("no keys given")
        );
        assert_eq!(parse_keys("country,").unwrap(), vec![PartitionKey::Country]);
        assert!(
            parse_keys("country,country")
                .unwrap_err()
                .to_string()
                .contains("country repeated")
        );
    }

    #[test]
    fn a_long_value_is_cut_with_a_hash_of_the_whole() {
        let a = "x".repeat(300) + "a";
        let b = "x".repeat(300) + "b";
        let (sa, changed) = sanitize(&a);
        let (sb, _) = sanitize(&b);
        assert!(changed);
        assert!(sa.len() <= MAX_SEGMENT, "{}", sa.len());
        assert_ne!(sa, sb);
        // Cut on a character boundary.
        let (s, _) = sanitize(&"é".repeat(150));
        assert!(s.len() <= MAX_SEGMENT);
    }

    #[test]
    fn slashes_are_sanitised_and_reported() {
        let mut report = Report::default();
        let mut claims = Claims::default();
        assert_eq!(segment("country", "A/B", &mut claims, &mut report), "A_B");
        assert_eq!(report.count("partition_value_sanitized"), 1);
    }

    #[test]
    fn colliding_values_get_distinct_segments() {
        let mut report = Report::default();
        let mut claims = Claims::default();
        let first = segment("country", "A/B", &mut claims, &mut report);
        let second = segment("country", "A_B", &mut claims, &mut report);
        assert_eq!(first, "A_B");
        assert_ne!(second, first);
        assert!(second.starts_with("A_B~"));
        assert_eq!(report.count("partition_value_collision"), 1);
        // Same value again reuses its segment.
        assert_eq!(segment("country", "A/B", &mut claims, &mut report), "A_B");
    }

    #[test]
    fn empty_and_nul_only_values_become_empty() {
        let mut report = Report::default();
        let mut claims = Claims::default();
        assert_eq!(segment("country", "", &mut claims, &mut report), "empty");
        assert_eq!(report.count("partition_value_sanitized"), 1);

        let mut report = Report::default();
        let mut claims = Claims::default();
        assert_eq!(segment("country", "\0", &mut claims, &mut report), "empty");
        assert_eq!(report.count("partition_value_sanitized"), 1);
    }

    #[test]
    fn sanitised_value_that_also_collides_reports_only_the_collision() {
        let mut report = Report::default();
        let mut claims = Claims::default();
        segment("country", "A_B", &mut claims, &mut report);
        let second = segment("country", "A/B", &mut claims, &mut report);
        assert!(second.starts_with("A_B~"));
        assert_eq!(report.count("partition_value_sanitized"), 0);
        assert_eq!(report.count("partition_value_collision"), 1);
    }

    #[test]
    fn hash_suffix_is_stable() {
        let mut report = Report::default();
        let mut claims = Claims::default();
        segment("country", "A_B", &mut claims, &mut report);
        let second = segment("country", "A/B", &mut claims, &mut report);
        assert_eq!(second, "A_B~ab5339ed");
    }

    #[test]
    fn case_only_twins_are_disambiguated() {
        let mut report = Report::default();
        let mut claims = Claims::default();
        let first = segment("country", "AB", &mut claims, &mut report);
        let second = segment("country", "ab", &mut claims, &mut report);
        assert_eq!(first, "AB");
        assert!(second.starts_with("ab~"));
        assert_eq!(report.count("partition_value_collision"), 1);
    }

    #[test]
    fn windows_hostile_characters_are_replaced() {
        let mut report = Report::default();
        let mut claims = Claims::default();
        assert_eq!(
            segment("country", "a:b*c", &mut claims, &mut report),
            "a_b_c"
        );
        assert_eq!(report.count("partition_value_sanitized"), 1);

        let mut report = Report::default();
        let mut claims = Claims::default();
        assert_eq!(segment("country", "CON", &mut claims, &mut report), "CON_");
        assert_eq!(report.count("partition_value_sanitized"), 1);

        let mut report = Report::default();
        let mut claims = Claims::default();
        assert_eq!(
            segment("country", "CON.txt", &mut claims, &mut report),
            "CON_.txt"
        );
        assert_eq!(report.count("partition_value_sanitized"), 1);

        let mut report = Report::default();
        let mut claims = Claims::default();
        assert_eq!(
            segment("country", "name.", &mut claims, &mut report),
            "name"
        );
        assert_eq!(report.count("partition_value_sanitized"), 1);
    }

    #[test]
    fn values_of_records_render_null_for_missing() {
        use crate::geometry::{GeomKind, GeometrySummary};
        use crate::index::IndexRecord;
        let rec = IndexRecord {
            country: "NG".into(),
            tier: "site".into(),
            type_code: None,
            geometry: Some(GeometrySummary {
                kind: GeomKind::Point,
                bbox: [0.0; 4],
            }),
            ..Default::default()
        };
        assert_eq!(PartitionKey::Country.value(&rec), "NG");
        assert_eq!(PartitionKey::GeomType.value(&rec), "point");
        assert_eq!(PartitionKey::Tier.value(&rec), "site");
        assert_eq!(PartitionKey::Type.value(&rec), "null");
    }
}
