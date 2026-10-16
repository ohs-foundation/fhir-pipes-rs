//! Data-quality issues found during a run. Never aborts; callers keep going.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One kind can fire once per row; at 100k rows that is a report larger than
/// the dataset. Keep this many per kind; counts stay exact regardless.
pub const MAX_RETAINED_ISSUES_PER_KIND: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Issue {
    pub kind: String,
    pub location_id: String,
    pub detail: String,
}

/// Serializable so a run can save it and carry on later (`fhir-pipes dhis2
/// reports --resume`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub issues: Vec<Issue>,
    counts: BTreeMap<String, usize>,
    retained: BTreeMap<String, usize>,
}

impl Report {
    pub fn add(&mut self, kind: &str, location_id: &str, detail: &str) {
        match self.counts.get_mut(kind) {
            Some(n) => *n += 1,
            None => {
                self.counts.insert(kind.to_string(), 1);
            }
        }
        if !self.retained.contains_key(kind) {
            self.retained.insert(kind.to_string(), 0);
        }
        let retained = self.retained.get_mut(kind).unwrap();
        if *retained < MAX_RETAINED_ISSUES_PER_KIND {
            self.issues.push(Issue {
                kind: kind.to_string(),
                location_id: location_id.to_string(),
                detail: detail.to_string(),
            });
            *retained += 1;
        }
    }

    /// Add another report's issues after this one's, as if they had been
    /// added here: the per-kind cap applies, and counts stay exact.
    pub fn append(&mut self, other: Report) {
        for issue in &other.issues {
            self.add(&issue.kind, &issue.location_id, &issue.detail);
        }
        for (kind, total) in other.counts {
            let kept = other.retained.get(&kind).copied().unwrap_or(0);
            *self.counts.entry(kind).or_default() += total - kept;
        }
    }

    pub fn counts(&self) -> &BTreeMap<String, usize> {
        &self.counts
    }

    #[cfg(test)]
    pub fn count(&self, kind: &str) -> usize {
        self.counts.get(kind).copied().unwrap_or(0)
    }

    /// Kinds where more issues occurred than were retained, with the omitted count.
    pub fn truncated(&self) -> BTreeMap<String, usize> {
        self.counts
            .iter()
            .filter_map(|(kind, total)| {
                let kept = self.retained.get(kind).copied().unwrap_or(0);
                (*total > kept).then(|| (kind.clone(), total - kept))
            })
            .collect()
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "counts": self.counts(),
            "issues": self.issues,
            "truncated": self.truncated(),
        })
    }

    pub fn summary(&self) -> String {
        if self.counts().is_empty() {
            return "No issues found.".to_string();
        }
        let mut out = String::from("Issues found:");
        for (kind, count) in self.counts() {
            out.push_str(&format!("\n  {kind}: {count}"));
        }
        let truncated = self.truncated();
        if !truncated.is_empty() {
            let omitted: Vec<String> = truncated
                .iter()
                .map(|(k, n)| format!("{k} (+{n} more)"))
                .collect();
            out.push_str(&format!(
                "\nissues list capped at {MAX_RETAINED_ISSUES_PER_KIND} per kind (counts above are exact); omitted from the list: {}",
                omitted.join(", ")
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_exact_but_retained_issues_are_capped() {
        let mut report = Report::default();
        for i in 0..(MAX_RETAINED_ISSUES_PER_KIND + 5) {
            report.add("orphan", &format!("loc-{i}"), "dangling");
        }
        assert_eq!(report.counts()["orphan"], MAX_RETAINED_ISSUES_PER_KIND + 5);
        assert_eq!(report.issues.len(), MAX_RETAINED_ISSUES_PER_KIND);
        assert_eq!(report.truncated()["orphan"], 5);
    }

    #[test]
    fn appending_keeps_counts_exact_and_the_cap() {
        let mut a = Report::default();
        a.add("orphan", "a", "");
        let mut b = Report::default();
        for i in 0..(MAX_RETAINED_ISSUES_PER_KIND + 2) {
            b.add("orphan", &format!("b-{i}"), "");
        }
        b.add("cycle", "c", "");
        a.append(b);
        assert_eq!(a.counts()["orphan"], MAX_RETAINED_ISSUES_PER_KIND + 3);
        assert_eq!(a.counts()["cycle"], 1);
        assert_eq!(a.issues[0].location_id, "a");
        assert_eq!(a.issues.len(), MAX_RETAINED_ISSUES_PER_KIND + 1);
        assert_eq!(a.truncated()["orphan"], 3);
    }

    #[test]
    fn summary_lists_kinds_sorted() {
        let mut report = Report::default();
        report.add("cycle", "b", "");
        report.add("orphan", "a", "");
        report.add("orphan", "c", "");
        assert_eq!(report.summary(), "Issues found:\n  cycle: 1\n  orphan: 2");
        assert_eq!(Report::default().summary(), "No issues found.");
    }

    #[test]
    fn summary_reports_truncation() {
        let mut report = Report::default();
        for i in 0..(MAX_RETAINED_ISSUES_PER_KIND + 3) {
            report.add("orphan", &format!("loc-{i}"), "dangling");
        }
        assert_eq!(
            report.summary(),
            "Issues found:\n  orphan: 1003\nissues list capped at 1000 per kind (counts above are exact); omitted from the list: orphan (+3 more)"
        );
    }

    #[test]
    fn json_shape_matches_python() {
        let mut report = Report::default();
        report.add("orphan", "a", "x");
        let json = report.to_json();
        assert_eq!(json["counts"]["orphan"], 1);
        assert_eq!(json["issues"][0]["kind"], "orphan");
        assert_eq!(json["issues"][0]["location_id"], "a");
        assert_eq!(json["issues"][0]["detail"], "x");
        assert!(json["truncated"].as_object().unwrap().is_empty());
    }
}
