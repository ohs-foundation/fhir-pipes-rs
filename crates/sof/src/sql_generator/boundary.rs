//! Temporal boundary helpers (lowBoundary/highBoundary) in SQL.

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
