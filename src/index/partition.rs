//! Which columns a row can be partitioned by, and how a value becomes a
//! filesystem-safe, collision-free `key=value` directory segment.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

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

/// A partition value as one path segment, in the `%XX` escapes hive uses and
/// DuckDB decodes when it reads the value from the directory name. Escaped:
/// what a path or a Windows file name cannot hold, control characters, `%`
/// itself, a trailing dot or space (Windows drops them), the first letter of
/// a Windows device name (`CON`, `CON.txt`: Windows reserves the part before
/// the first `.`), and with `letters` every ASCII letter.
fn escape(v: &str, letters: bool) -> String {
    let base = v.split('.').next().unwrap_or(v);
    let device = RESERVED_WINDOWS_NAMES
        .iter()
        .any(|n| n.eq_ignore_ascii_case(base));
    let trailing = v.len() - v.trim_end_matches(['.', ' ']).len();
    let mut out = String::with_capacity(v.len());
    for (i, c) in v.char_indices() {
        let escaped = matches!(
            c,
            '/' | '\\' | '=' | '%' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
        ) || c.is_control()
            || i >= v.len() - trailing
            || (device && i == 0)
            || (letters && c.is_ascii_alphabetic());
        if escaped {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                let _ = write!(out, "%{b:02X}");
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A directory name's partition value: `escape` undone.
pub(crate) fn unescape(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One partition value as a directory name (`escape`), safe on the strictest
/// common target (a dataset written on Linux must survive copying to
/// Windows or a case-insensitive macOS volume).
///
/// Returns `(candidate, lossy)`: `lossy` when the value cannot be read back
/// from the name. An empty value becomes `"empty"`, and a name longer than
/// `MAX_SEGMENT` is cut, with a hash of the whole value to keep two values
/// that start the same apart.
fn render(value: &str, letters: bool) -> (String, bool) {
    if value.is_empty() {
        return ("empty".to_string(), true);
    }
    let result = escape(value, letters);
    // A file name is at most 255 bytes on most filesystems, and `key=` and
    // a collision suffix come on top.
    if result.len() <= MAX_SEGMENT {
        return (result, false);
    }
    let mut cut = MAX_SEGMENT - 9;
    while !result.is_char_boundary(cut) {
        cut -= 1;
    }
    // Not inside a `%XX`: every `%` in the name starts one.
    if let Some(p) = result[cut.saturating_sub(2)..cut].find('%') {
        cut -= 2 - p;
    }
    let digest = Sha256::digest(value.as_bytes());
    let result = format!(
        "{}~{:02x}{:02x}{:02x}{:02x}",
        &result[..cut],
        digest[0],
        digest[1],
        digest[2],
        digest[3]
    );
    (result, true)
}

/// The longest partition directory name written whole, in bytes.
const MAX_SEGMENT: usize = 200;

/// Segment claims within one parent directory, keyed case-insensitively:
/// lowercased segment -> (original-case segment, owning value). A
/// case-insensitive filesystem (Windows, default macOS) would otherwise
/// merge two directories that differ only in case with no report.
#[derive(Debug, Default)]
pub struct Claims(HashMap<String, (String, String)>);

/// What `Claims::take` found.
#[derive(PartialEq, Eq)]
enum Claim {
    /// The segment was free and is the value's now.
    New,
    /// The value had it already.
    Mine,
    /// Another value has it.
    Taken,
}

impl Claims {
    fn take(&mut self, segment: &str, value: &str) -> Claim {
        match self.0.get(&segment.to_lowercase()) {
            Some((_, owner)) if owner == value => Claim::Mine,
            Some(_) => Claim::Taken,
            None => {
                self.0.insert(
                    segment.to_lowercase(),
                    (segment.to_string(), value.to_string()),
                );
                Claim::New
            }
        }
    }
}

/// Render one partition value as a filesystem-safe, collision-free
/// directory name segment (without the `key=` prefix), scoped to `claims`
/// (one `Claims` per parent-directory-and-key context). The views step of
/// `refresh` and the Location dataset both name their partitions with this.
///
/// Two distinct values that render to the same segment -- case-insensitively
/// -- must not silently share a directory. The second gets every letter
/// escaped (`Kano`, then `%4B%41%4E%4F` for `KANO`), which still reads back
/// as the value; where that is taken too, or the value was cut, it gets a
/// short deterministic hash of its own candidate and value appended. What
/// cannot be read back from the directory name is reported: a value that is
/// empty or too long as `partition_value_sanitized`, a hash suffix as
/// `partition_value_collision` (only that, when both apply).
pub fn segment(key: &str, value: &str, claims: &mut Claims, report: &mut Report) -> String {
    let (candidate, lossy) = render(value, false);
    match claims.take(&candidate, value) {
        Claim::Mine => return candidate,
        Claim::New => {
            if lossy {
                report.add(
                    "partition_value_sanitized",
                    &format!("{key}={value}"),
                    &format!(
                        "cannot be a directory name as it is (empty or more than {MAX_SEGMENT} bytes); written to the directory {key}={candidate:?} instead"
                    ),
                );
            }
            return candidate;
        }
        Claim::Taken => {}
    }
    let (letters, lossy) = render(value, true);
    if !lossy && claims.take(&letters, value) != Claim::Taken {
        return letters;
    }
    let mut salt = String::new();
    loop {
        let digest = Sha256::digest(format!("{candidate}\0{value}{salt}").as_bytes());
        let seg = format!(
            "{candidate}~{:02x}{:02x}{:02x}{:02x}",
            digest[0], digest[1], digest[2], digest[3]
        );
        match claims.take(&seg, value) {
            Claim::Mine => return seg,
            Claim::Taken => salt.push('#'),
            Claim::New => {
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
        let (sa, lossy) = render(&a, false);
        let (sb, _) = render(&b, false);
        assert!(lossy);
        assert!(sa.len() <= MAX_SEGMENT, "{}", sa.len());
        assert_ne!(sa, sb);
        // Cut on a character boundary, and not inside an escape.
        let (s, _) = render(&"é".repeat(150), false);
        assert!(s.len() <= MAX_SEGMENT);
        for n in 0..3 {
            let (s, _) = render(&("x".repeat(n) + &"/".repeat(100)), false);
            let name = s.split('~').next().unwrap();
            assert!(name.ends_with("%2F"), "{name}");
        }
    }

    #[test]
    fn values_are_escaped_as_path_segments() {
        assert_eq!(escape("SLE", false), "SLE");
        assert_eq!(escape("a/b=c", false), "a%2Fb%3Dc");
        assert_eq!(escape("a*b?", false), "a%2Ab%3F");
        assert_eq!(escape("x. ", false), "x%2E%20");
        assert_eq!(escape("con.txt", false), "%63on.txt");
        assert_eq!(escape("é\n", false), "é%0A");
        assert_eq!(escape("Ab1", true), "%41%621");
        for v in ["SLE", "a/b=c", "a*b?", "x. ", "con.txt", "é\n", "50%", "\0"] {
            assert_eq!(unescape(&escape(v, false)), v);
            assert_eq!(unescape(&escape(v, true)), v);
        }
    }

    #[test]
    fn escaped_values_are_not_reported() {
        let mut report = Report::default();
        let mut claims = Claims::default();
        assert_eq!(segment("country", "A/B", &mut claims, &mut report), "A%2FB");
        assert_eq!(
            segment("country", "a:b*c", &mut claims, &mut report),
            "a%3Ab%2Ac"
        );
        assert_eq!(segment("country", "CON", &mut claims, &mut report), "%43ON");
        assert_eq!(
            segment("country", "CON.txt", &mut claims, &mut report),
            "%43ON.txt"
        );
        assert_eq!(
            segment("country", "name.", &mut claims, &mut report),
            "name%2E"
        );
        assert_eq!(segment("country", "\0", &mut claims, &mut report), "%00");
        assert_eq!(report.count("partition_value_sanitized"), 0);
        assert_eq!(report.count("partition_value_collision"), 0);
    }

    #[test]
    fn an_empty_value_is_named_empty_and_reported() {
        let mut report = Report::default();
        let mut claims = Claims::default();
        assert_eq!(segment("country", "", &mut claims, &mut report), "empty");
        assert_eq!(report.count("partition_value_sanitized"), 1);
        // The value "empty" is then told apart by its escaped letters.
        assert_eq!(
            segment("country", "empty", &mut claims, &mut report),
            "%65%6D%70%74%79"
        );
        assert_eq!(segment("country", "", &mut claims, &mut report), "empty");
        assert_eq!(report.count("partition_value_sanitized"), 1);
    }

    #[test]
    fn values_that_differ_only_in_case_get_directories_of_their_own() {
        let mut report = Report::default();
        let mut claims = Claims::default();
        let got: Vec<String> = ["SLE", "Sle", "sle", "SLE", "sle"]
            .iter()
            .map(|v| segment("country", v, &mut claims, &mut report))
            .collect();
        assert_eq!(got, ["SLE", "%53%6C%65", "%73%6C%65", "SLE", "%73%6C%65"]);
        // Each reads back as its value: nothing to report.
        assert_eq!(report.count("partition_value_collision"), 0);
    }

    #[test]
    fn a_hash_tells_apart_what_escaping_cannot() {
        // Only ASCII letters are escaped, so `É` and `é` still share a
        // directory on a case-insensitive disk.
        let mut report = Report::default();
        let mut claims = Claims::default();
        assert_eq!(segment("country", "É", &mut claims, &mut report), "É");
        let second = segment("country", "é", &mut claims, &mut report);
        assert_eq!(second, "é~2c36a6c7");
        assert_eq!(report.count("partition_value_collision"), 1);
        // Same value again reuses its segment, with no second report.
        assert_eq!(segment("country", "é", &mut claims, &mut report), second);
        assert_eq!(report.count("partition_value_collision"), 1);
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
