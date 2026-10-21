//! The values of one unit of work: analytics requests and their responses,
//! or the same tables computed from data values. A request DHIS2 calls too
//! large is split in two and asked again.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::dhis2::api::Dhis2;
use crate::dhis2::measures::Indicator;
use crate::dhis2::raw::{RawSource, Span};
use crate::error::{PipesError, Result};
use crate::http::FetchError;

use super::periods::{DateRange, periods_label};
use super::plan::Unit;

/// One analytics response: rows by column name, and each period's dates.
#[derive(Debug, Default)]
pub struct Analytics {
    pub rows: Vec<AnalyticsRow>,
    /// Period code → (start, end) as `YYYY-MM-DD`.
    pub periods: HashMap<String, (String, String)>,
    /// Display name of every item in the response (dimensions and their options).
    pub names: HashMap<String, String>,
    /// Every org unit and period the request covered, data or not, as
    /// DHIS2 lists them in `metaData.dimensions` (relative periods expanded).
    pub org_units: Vec<String>,
    pub period_codes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnalyticsRow {
    pub dx: String,
    pub pe: String,
    pub ou: String,
    pub value: String,
    pub numerator: String,
    pub denominator: String,
    /// The breakdown option (category option, org unit group, …), when the
    /// request had a breakdown dimension.
    pub option: Option<String>,
}

/// A request by date range has no `pe` column; its rows get the range's code
/// from the caller.
pub fn parse_analytics(v: &Value, breakdown: Option<&str>) -> Result<Analytics> {
    let headers: Vec<&str> = v
        .get("headers")
        .and_then(Value::as_array)
        .ok_or_else(|| PipesError::Environment("analytics response has no headers".into()))?
        .iter()
        .map(|h| h.get("name").and_then(Value::as_str).unwrap_or(""))
        .collect();
    let col = |name: &str| headers.iter().position(|h| *h == name);
    let need = |name: &str| {
        col(name).ok_or_else(|| {
            PipesError::Environment(format!("analytics response has no {name} column"))
        })
    };
    let (dx, pe, ou, value) = (need("dx")?, col("pe"), need("ou")?, need("value")?);
    let (num, den) = (col("numerator"), col("denominator"));
    let opt = match breakdown {
        Some(b) => Some(need(b)?),
        None => None,
    };
    let cell = |row: &[Value], i: Option<usize>| -> String {
        i.and_then(|i| row.get(i))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let rows = v
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .map(|r| AnalyticsRow {
            dx: cell(r, Some(dx)),
            pe: cell(r, pe),
            ou: cell(r, Some(ou)),
            value: cell(r, Some(value)),
            numerator: cell(r, num),
            denominator: cell(r, den),
            option: opt.map(|i| cell(r, Some(i))),
        })
        .collect();
    let mut periods = HashMap::new();
    let mut names = HashMap::new();
    if let Some(items) = v.pointer("/metaData/items").and_then(Value::as_object) {
        for (code, item) in items {
            if let Some(n) = item.get("name").and_then(Value::as_str) {
                names.insert(code.clone(), n.to_string());
            }
            let date = |k: &str| {
                item.get(k)
                    .and_then(Value::as_str)
                    .and_then(|s| s.get(..10))
                    .map(str::to_string)
            };
            if let (Some(start), Some(end)) = (date("startDate"), date("endDate")) {
                periods.insert(code.clone(), (start, end));
            }
        }
    }
    let dimension = |d: &str| -> Vec<String> {
        v.pointer(&format!("/metaData/dimensions/{d}"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    };
    Ok(Analytics {
        rows,
        periods,
        names,
        org_units: dimension("ou"),
        period_codes: dimension("pe"),
    })
}

/// DHIS2 refuses analytics queries whose result is over its row limit
/// (E7128) or over the table-layout limit (E7151).
fn too_large(e: &FetchError) -> bool {
    matches!(e, FetchError::Status { status: 409, body } if body.contains("E7128") || body.contains("E7151"))
}

/// One analytics request; the parts a too-large result is split along.
#[derive(Clone, Copy)]
struct Ask<'a> {
    level: u32,
    root: Option<&'a str>,
    periods: &'a [String],
    range: Option<&'a DateRange>,
    indicators: &'a [String],
    breakdown: Option<&'a str>,
}

impl Ask<'_> {
    fn when(&self) -> String {
        match self.range {
            Some(r) => r.label(),
            None => periods_label(self.periods),
        }
    }

    fn label(&self) -> String {
        format!(
            "level {} {}{}",
            self.level,
            self.when(),
            self.breakdown
                .map(|b| format!(" by {b}"))
                .unwrap_or_default()
        )
    }
}

/// A unit's analytics responses (or the same tables computed from data
/// values): the totals, then each breakdown's.
#[derive(Debug, Default)]
pub(super) struct Fetched {
    pub(super) totals: Vec<Analytics>,
    pub(super) breakdowns: Vec<(String, Vec<Analytics>)>,
    pub(super) requests: usize,
    /// Issues met while computing, as (kind, key, detail).
    pub(super) issues: Vec<(String, String, String)>,
}

/// Where the values come from.
#[derive(Clone, Copy)]
pub(super) enum Values<'a> {
    Analytics {
        root: Option<&'a str>,
    },
    DataValues {
        raw: &'a RawSource,
        indicators: &'a BTreeMap<String, Indicator>,
    },
}

pub(super) fn fetch_unit(
    api: &Dhis2,
    values: Values,
    breakdowns: &[String],
    unit: &Unit,
) -> Result<Fetched> {
    let root = match values {
        Values::Analytics { root } => root,
        Values::DataValues { raw, indicators } => {
            let targets: Vec<Span> = match &unit.range {
                Some(r) => Span::of_range(r).into_iter().collect(),
                None => unit
                    .periods
                    .iter()
                    .filter_map(|p| Span::of_period(p))
                    .collect(),
            };
            let items: Vec<&Indicator> = unit
                .indicators
                .iter()
                .filter_map(|i| indicators.get(i))
                .collect();
            let (totals, by, issues) = raw.compute(api, &items, &targets)?;
            return Ok(Fetched {
                totals: vec![totals],
                breakdowns: breakdowns
                    .iter()
                    .cloned()
                    .zip(by.into_iter().map(|t| vec![t]))
                    .collect(),
                requests: 1,
                issues,
            });
        }
    };
    let mut f = Fetched::default();
    let ask = Ask {
        level: unit.levels[0],
        root,
        periods: &unit.periods,
        range: unit.range.as_ref(),
        indicators: &unit.indicators,
        breakdown: None,
    };
    fetch(api, ask, &mut f.requests, &mut f.totals)?;
    for b in breakdowns {
        let mut tables = Vec::new();
        let ask = Ask {
            breakdown: Some(b),
            ..ask
        };
        fetch(api, ask, &mut f.requests, &mut tables)?;
        f.breakdowns.push((b.clone(), tables));
    }
    Ok(f)
}

/// Asks DHIS2, splitting the request in two and asking again while DHIS2
/// says the result is too large: the periods first, then the indicators.
/// A date range is sent as `startDate` and `endDate` (DHIS2 takes them
/// instead of a period dimension, never with one).
fn fetch(api: &Dhis2, ask: Ask, requests: &mut usize, out: &mut Vec<Analytics>) -> Result<()> {
    let when = match ask.range {
        Some(r) => [("startDate", r.start.clone()), ("endDate", r.end.clone())].to_vec(),
        None => vec![("dimension", format!("pe:{}", ask.periods.join(";")))],
    };
    let mut query = vec![("dimension", format!("dx:{}", ask.indicators.join(";")))];
    query.extend(when);
    query.extend([
        (
            "dimension",
            match ask.root {
                Some(root) => format!("ou:LEVEL-{};{root}", ask.level),
                None => format!("ou:LEVEL-{}", ask.level),
            },
        ),
        ("includeNumDen", "true".into()),
        ("skipRounding", "true".into()),
        ("includeMetadataDetails", "true".into()),
        ("displayProperty", "NAME".into()),
    ]);
    if let Some(b) = ask.breakdown {
        query.push(("dimension", b.to_string()));
    }
    *requests += 1;
    let body = match api.try_get("analytics.json", &query)? {
        Ok(v) => v,
        Err(e) if too_large(&e) && ask.periods.len() > 1 => {
            let (a, b) = ask.periods.split_at(ask.periods.len() / 2);
            eprintln!(
                "{}: result too large for DHIS2, splitting {} periods in two",
                ask.label(),
                ask.periods.len()
            );
            fetch(api, Ask { periods: a, ..ask }, requests, out)?;
            return fetch(api, Ask { periods: b, ..ask }, requests, out);
        }
        Err(e) if too_large(&e) && ask.indicators.len() > 1 => {
            let (a, b) = ask.indicators.split_at(ask.indicators.len() / 2);
            eprintln!(
                "{}: result too large for DHIS2, splitting {} indicators in two",
                ask.label(),
                ask.indicators.len()
            );
            fetch(
                api,
                Ask {
                    indicators: a,
                    ..ask
                },
                requests,
                out,
            )?;
            return fetch(
                api,
                Ask {
                    indicators: b,
                    ..ask
                },
                requests,
                out,
            );
        }
        Err(e) if too_large(&e) => {
            return Err(PipesError::Usage(format!(
                "indicator {} at level {} for {} is over DHIS2's analytics row limit on its own; ask for shorter periods ({e})",
                ask.indicators[0],
                ask.level,
                ask.when()
            )));
        }
        Err(FetchError::Status { status: 409, body }) => {
            return Err(PipesError::Usage(format!(
                "DHIS2 rejected the analytics query for level {} and period {}: {body}",
                ask.level,
                match ask.range {
                    Some(r) => r.label(),
                    None => ask.periods.join(";"),
                }
            )));
        }
        Err(e) => return Err(api.error("analytics.json", e)),
    };
    let mut table = parse_analytics(&body, ask.breakdown)?;
    if let Some(r) = ask.range {
        let code = r.code();
        for row in &mut table.rows {
            row.pe.clone_from(&code);
        }
        table.periods.insert(code, (r.start.clone(), r.end.clone()));
    }
    out.push(table);
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use serde_json::json;

    use super::*;

    pub(crate) fn analytics() -> Value {
        json!({
            "headers": [{"name":"dx"},{"name":"pe"},{"name":"ou"},{"name":"value"},{"name":"numerator"},
                        {"name":"denominator"},{"name":"factor"},{"name":"multiplier"},{"name":"divisor"}],
            "metaData": {"items": {
                "2025": {"name":"2025","startDate":"2025-01-01T00:00:00.000","endDate":"2025-12-31T00:00:00.000"},
                "O6uvpzGd5pu": {"name":"Bo"}
            }},
            "rows": [["tUIlpyeeX9N","2025","O6uvpzGd5pu","27.9764","62045.0","221781.0","100.0","36500","365"]]
        })
    }

    #[test]
    fn analytics_rows_and_period_dates_are_read_by_column_name() {
        let t = parse_analytics(&analytics(), None).unwrap();
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.rows[0].ou, "O6uvpzGd5pu");
        assert_eq!(t.rows[0].numerator, "62045.0");
        assert_eq!(
            t.periods.get("2025"),
            Some(&("2025-01-01".to_string(), "2025-12-31".to_string()))
        );
        assert!(!t.periods.contains_key("O6uvpzGd5pu"));
        assert!(parse_analytics(&json!({"rows": []}), None).is_err());
    }

    #[test]
    fn a_response_by_date_range_has_no_period_column() {
        let v = json!({
            "headers": [{"name":"dx"},{"name":"ou"},{"name":"value"},{"name":"numerator"},{"name":"denominator"}],
            "metaData": {"items": {}},
            "rows": [["tUIlpyeeX9N","O6uvpzGd5pu","50.0","5.0","10.0"]]
        });
        let t = parse_analytics(&v, None).unwrap();
        assert_eq!(t.rows[0].pe, "");
        assert_eq!(t.rows[0].numerator, "5.0");
    }

    #[test]
    fn only_row_limit_errors_count_as_too_large() {
        let e = |status: u16, body: &str| FetchError::Status {
            status,
            body: body.into(),
        };
        assert!(too_large(&e(
            409,
            r#"{"errorCode":"E7128","message":"Query result set exceeded max limit"}"#
        )));
        assert!(too_large(&e(409, "E7151")));
        assert!(!too_large(&e(409, "E7124")));
        assert!(!too_large(&e(500, "E7128")));
    }
}
