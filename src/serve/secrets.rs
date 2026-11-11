//! The values `serve` hides: every secret it knows of is replaced with
//! `***` in each log line before it is written anywhere, in the strings of
//! run records, and in notify bodies.

use serde_json::Value;

use super::config::ServeConfig;

/// A value shorter than this is not replaced: it would replace ordinary
/// text everywhere.
pub const MIN_SECRET: usize = 4;

/// The variables whose values are secrets, besides the `[database]`
/// section's and each notify destination's.
const SECRET_VARS: [&str; 6] = [
    "FHIR_PIPES_TOKEN",
    "FHIR_PIPES_CLIENT_SECRET",
    "FHIR_PIPES_PASSWORD",
    "FHIR_PIPES_HAPI_DATABASE_URL",
    "FHIR_PIPES_SERVE_TOKEN",
    "FHIR_PIPES_QUERY_TOKEN",
];

#[derive(Debug, Default)]
pub struct Redactor {
    /// Longest first, so a value inside another is not matched first.
    values: Vec<Vec<u8>>,
}

impl Redactor {
    /// From (where it came from, value) pairs: each line of a value is a
    /// value to hide. Returns the warnings for values too short to hide.
    pub fn new(values: impl IntoIterator<Item = (String, String)>) -> (Redactor, Vec<String>) {
        let mut kept: Vec<Vec<u8>> = Vec::new();
        let mut warnings = Vec::new();
        for (from, value) in values {
            if value.len() < MIN_SECRET {
                warnings.push(format!(
                    "{from} is shorter than {MIN_SECRET} characters, so serve cannot hide it in logs and records"
                ));
                continue;
            }
            for line in value
                .lines()
                .map(str::trim_end)
                .filter(|l| l.len() >= MIN_SECRET)
            {
                kept.push(line.as_bytes().to_vec());
            }
        }
        kept.sort_by_key(|v| std::cmp::Reverse(v.len()));
        kept.dedup();
        (Redactor { values: kept }, warnings)
    }

    /// The secrets in serve's environment and config.
    pub fn from_env(cfg: &ServeConfig, database_url_env: Option<&str>) -> (Redactor, Vec<String>) {
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut var = |name: &str| {
            if let Ok(v) = std::env::var(name)
                && !v.is_empty()
            {
                pairs.push((name.to_string(), v));
            }
        };
        for name in SECRET_VARS {
            var(name);
        }
        if let Some(name) = database_url_env {
            var(name);
        }
        for d in &cfg.notify {
            if let Some((_, env)) = &d.header {
                var(env);
            }
        }
        for d in &cfg.notify {
            pairs.push((format!("notify {} url", d.name), d.url.to_string()));
        }
        Redactor::new(pairs)
    }

    /// The length of the longest value, in bytes.
    pub fn longest(&self) -> usize {
        self.values.first().map_or(0, Vec::len)
    }

    /// Where whole values start in `input`, leftmost first, without overlaps:
    /// (start, length).
    pub fn matches(&self, input: &[u8]) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        if self.values.is_empty() {
            return found;
        }
        let mut pos = 0;
        while pos < input.len() {
            // The longest value that starts here, else the next position.
            match self.values.iter().find(|v| input[pos..].starts_with(v)) {
                Some(v) => {
                    found.push((pos, v.len()));
                    pos += v.len();
                }
                None => pos += 1,
            }
        }
        found
    }

    pub fn redact(&self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut pos = 0;
        for (start, len) in self.matches(s.as_bytes()) {
            out.push_str(&s[pos..start]);
            out.push_str("***");
            pos = start + len;
        }
        out.push_str(&s[pos..]);
        out
    }

    /// Every string in `v`, keys included, before it is serialised.
    pub fn redact_json(&self, v: &mut Value) {
        match v {
            Value::String(s) => *s = self.redact(s),
            Value::Array(a) => a.iter_mut().for_each(|x| self.redact_json(x)),
            Value::Object(o) => {
                let taken = std::mem::take(o);
                for (k, mut x) in taken {
                    self.redact_json(&mut x);
                    o.insert(self.redact(&k), x);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn r(values: &[&str]) -> Redactor {
        Redactor::new(values.iter().map(|v| ("X".to_string(), v.to_string()))).0
    }

    #[test]
    fn values_are_replaced_whole_longest_first() {
        let r = r(&["abcd", "abcdefgh"]);
        assert_eq!(r.redact("x abcdefgh y abcd z"), "x *** y *** z");
        assert_eq!(r.longest(), 8);
    }

    #[test]
    fn a_multiline_secret_is_replaced_line_by_line() {
        let r = r(&["first-line\nsecond-line"]);
        assert_eq!(r.redact("a first-line b"), "a *** b");
        assert_eq!(r.redact("second-line"), "***");
    }

    #[test]
    fn a_short_secret_warns() {
        let (r, warnings) = Redactor::new([("FHIR_PIPES_PASSWORD".to_string(), "pw".to_string())]);
        assert_eq!(r.redact("pw"), "pw");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("FHIR_PIPES_PASSWORD"), "{warnings:?}");
    }

    #[test]
    fn json_strings_are_redacted_before_serialising() {
        let r = r(&["se\"cret"]);
        let mut v = json!({"error": "token se\"cret refused", "n": 1, "list": ["se\"cret"]});
        r.redact_json(&mut v);
        let text = v.to_string();
        assert!(!text.contains("se\\\"cret"), "{text}");
        assert_eq!(v["error"], "token *** refused");
        assert_eq!(v["list"][0], "***");
    }
}
