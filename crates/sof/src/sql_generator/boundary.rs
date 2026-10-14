//! Temporal boundary helpers (lowBoundary/highBoundary) and type inference.

use octofhir_fhirpath::ExpressionNode;

use super::ddl::Dialect;

/// SQL expression widening a `YYYY-MM` string `t` to that month's last day in
/// `YYYY-MM-DD` form, for the chosen dialect.
fn end_of_month(dialect: Dialect, t: &str) -> String {
    if matches!(dialect, Dialect::DuckDb) {
        format!("strftime(strptime({t}, '%Y-%m') + INTERVAL 1 MONTH - INTERVAL 1 DAY, '%Y-%m-%d')")
    } else {
        format!(
            "to_char(to_date({t}, 'YYYY-MM') + interval '1 month' - interval '1 day', 'YYYY-MM-DD')"
        )
    }
}

/// The FHIR temporal type a boundary function operates on, inferred from a
/// leading `ofType(T)` where present.
#[derive(Debug, Clone, Copy)]
pub(super) enum BoundaryType {
    Date,
    DateTime,
    Time,
    Unknown,
}

pub(super) fn boundary_hint(object: &ExpressionNode) -> BoundaryType {
    match object {
        ExpressionNode::MethodCall(m) if m.method == "ofType" => {
            let name = match m.arguments.first() {
                Some(ExpressionNode::Identifier(n)) => n.name.to_lowercase(),
                Some(ExpressionNode::TypeInfo(t)) => t.name.to_lowercase(),
                _ => return BoundaryType::Unknown,
            };
            match name.as_str() {
                "datetime" | "instant" => BoundaryType::DateTime,
                "date" => BoundaryType::Date,
                "time" => BoundaryType::Time,
                _ => BoundaryType::Unknown,
            }
        }
        ExpressionNode::Parenthesized(e) => boundary_hint(e),
        _ => BoundaryType::Unknown,
    }
}

/// Widen a partial date string `t` to its low/high full-date bound.
pub(super) fn date_bound(dialect: Dialect, t: &str, low: bool) -> String {
    if low {
        format!(
            "CASE length({t}) WHEN 4 THEN {t} || '-01-01' WHEN 7 THEN {t} || '-01' ELSE {t} END"
        )
    } else {
        format!(
            "CASE length({t}) \
               WHEN 4 THEN {t} || '-12-31' \
               WHEN 7 THEN {eom} \
               ELSE {t} END",
            eom = end_of_month(dialect, t)
        )
    }
}

/// Widen a partial dateTime string `t`, using the timezone extremes (+14:00 for
/// the earliest instant, -12:00 for the latest) the spec mandates.
pub(super) fn datetime_bound(dialect: Dialect, t: &str, low: bool) -> String {
    if low {
        format!(
            "CASE length({t}) \
               WHEN 4 THEN {t} || '-01-01T00:00:00.000+14:00' \
               WHEN 7 THEN {t} || '-01T00:00:00.000+14:00' \
               WHEN 10 THEN {t} || 'T00:00:00.000+14:00' \
               ELSE {t} END"
        )
    } else {
        format!(
            "CASE length({t}) \
               WHEN 4 THEN {t} || '-12-31T23:59:59.999-12:00' \
               WHEN 7 THEN {eom} || 'T23:59:59.999-12:00' \
               WHEN 10 THEN {t} || 'T23:59:59.999-12:00' \
               ELSE {t} END",
            eom = end_of_month(dialect, t)
        )
    }
}

/// Widen a partial time string `t` to its low/high bound.
pub(super) fn time_bound(_dialect: Dialect, t: &str, low: bool) -> String {
    if low {
        format!(
            "CASE length({t}) WHEN 2 THEN {t} || ':00:00.000' WHEN 5 THEN {t} || ':00.000' WHEN 8 THEN {t} || '.000' ELSE {t} END"
        )
    } else {
        format!(
            "CASE length({t}) WHEN 2 THEN {t} || ':59:59.999' WHEN 5 THEN {t} || ':59.999' WHEN 8 THEN {t} || '.999' ELSE {t} END"
        )
    }
}

pub(super) fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
