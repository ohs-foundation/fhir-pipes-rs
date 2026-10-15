//! Rustc-style rendering of lint findings for the CLI.
//!
//! Uses `annotate-snippets` (the renderer the Rust compiler itself uses) to
//! print each finding with a `severity[CODE]:` header, the offending source
//! line from the ViewDefinition JSON, and a caret underline. Findings carry a
//! selector or column name rather than a byte offset, so the span is recovered
//! by locating that quoted string in the source.

use std::io::IsTerminal;

use annotate_snippets::{Level, Renderer, Snippet};
use fhir_pipes_sof_lint::{Finding, Severity};

/// Whether coloured output should be used: not forced off, `NO_COLOR` unset,
/// and the stream is a TTY.
pub fn use_color(force_no_color: bool) -> bool {
    if force_no_color || std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

fn level_of(severity: Severity) -> Level {
    match severity {
        Severity::Error => Level::Error,
        Severity::Warning => Level::Warning,
    }
}

/// Find the byte range of a finding's location (a column name or selector) by
/// locating its quoted occurrence in the source. Returns the range inside the
/// quotes.
fn find_span(source: &str, location: &str) -> Option<(usize, usize)> {
    let needle = format!("\"{location}\"");
    let at = source.find(&needle)?;
    let start = at + 1;
    Some((start, start + location.len()))
}

/// Render findings against the source as a rustc-style report.
pub fn render_findings(origin: &str, source: &str, findings: &[Finding], color: bool) -> String {
    let renderer = if color {
        Renderer::styled()
    } else {
        Renderer::plain()
    };

    let mut out = String::new();
    for f in findings {
        let level = level_of(f.severity);
        let span = f.location.as_deref().and_then(|loc| find_span(source, loc));
        let help = f.help_url.as_ref().map(|u| format!("see {u}"));

        let mut message = level.title(&f.message).id(&f.code);
        if let Some((start, end)) = span {
            let start = start.min(source.len());
            let end = end.min(source.len()).max(start);
            let snippet = Snippet::source(source)
                .origin(origin)
                .fold(true)
                .annotation(level.span(start..end));
            message = message.snippet(snippet);
        }
        if let Some(help) = &help {
            message = message.footer(Level::Help.title(help));
        }
        out.push_str(&renderer.render(message).to_string());
        out.push('\n');
    }
    out
}

/// Render findings as a machine-readable JSON array.
pub fn render_findings_json(findings: &[Finding]) -> String {
    let items: Vec<serde_json::Value> = findings
        .iter()
        .map(|f| {
            serde_json::json!({
                "code": f.code,
                "severity": f.severity.to_string(),
                "message": f.message,
                "location": f.location,
                "helpUrl": f.help_url,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::Value::Array(items)).unwrap_or_default()
}

/// Render findings as SARIF 2.1.0 for CI code-scanning ingestion. `origin` is
/// the analysed file, used as the result artifact URI.
pub fn render_findings_sarif(origin: &str, findings: &[Finding]) -> String {
    use std::collections::BTreeMap;

    // Unique rules (id -> helpUri) for tool.driver.rules.
    let mut rules: BTreeMap<&str, Option<&str>> = BTreeMap::new();
    for f in findings {
        rules
            .entry(f.code.as_str())
            .or_insert(f.help_url.as_deref());
    }
    let rule_objs: Vec<serde_json::Value> = rules
        .iter()
        .map(|(id, help)| {
            let mut r = serde_json::json!({ "id": id });
            if let Some(uri) = help {
                r["helpUri"] = serde_json::json!(uri);
            }
            r
        })
        .collect();

    let results: Vec<serde_json::Value> = findings
        .iter()
        .map(|f| {
            let level = match f.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            };
            let mut result = serde_json::json!({
                "ruleId": f.code,
                "level": level,
                "message": { "text": f.message },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": origin }
                    }
                }],
            });
            if let Some(loc) = &f.location {
                result["locations"][0]["logicalLocations"] = serde_json::json!([{ "name": loc }]);
            }
            result
        })
        .collect();

    let sarif = serde_json::json!({
        "version": "2.1.0",
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "octofhir-sof",
                    "informationUri": "https://github.com/octofhir/sof",
                    "version": env!("CARGO_PKG_VERSION"),
                    "rules": rule_objs,
                }
            },
            "results": results,
        }],
    });
    serde_json::to_string_pretty(&sarif).unwrap_or_default()
}

/// A green status line (e.g. `valid`), plain when colour is off.
pub fn ok(msg: &str, color: bool) -> String {
    paint(msg, "32", color)
}

/// A green/red PASS or FAIL marker.
pub fn marker(pass: bool, color: bool) -> String {
    if pass {
        paint("PASS", "32", color)
    } else {
        paint("FAIL", "31", color)
    }
}

fn paint(msg: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{msg}\x1b[0m")
    } else {
        msg.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fhir_pipes_sof_lint::Finding;

    #[test]
    fn caret_when_locatable() {
        let source = r#"{"select":[{"column":[{"name":"id","path":"name.family"}]}]}"#;
        let findings =
            vec![Finding::fhir("FH04", Severity::Error, "array into scalar").at("name.family")];
        let out = render_findings("view.json", source, &findings, false);
        assert!(out.contains("FH04"));
        assert!(out.contains("^"), "expected a caret underline:\n{out}");
        assert!(out.contains("name.family"));
    }

    #[test]
    fn falls_back_without_location() {
        let findings = vec![Finding::fhir("FH10", Severity::Error, "no columns")];
        let out = render_findings("view.json", "{}", &findings, false);
        assert!(out.contains("FH10"));
        assert!(out.contains("no columns"));
        assert!(!out.contains("^"));
    }
}
