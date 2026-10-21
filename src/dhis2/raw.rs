//! `fhir-pipes dhis2 reports --source data-values`: values computed by fhir-pipes from
//! raw data values (`/api/dataValueSets`) instead of DHIS2's analytics.
//!
//! Each batch's data values are added up the org unit hierarchy (the
//! `partOf` chain of the `--locations` file) to every requested level, and
//! each item is then computed the way DHIS2 analytics computes it:
//!
//! - A data element's value is its total over category option combos and
//!   attribute option combos, over the period and every org unit under the
//!   reported one. Aggregation types SUM and AVERAGE_SUM_ORG_UNIT are
//!   followed (the latter averages each org unit's values over the period,
//!   per category option combo, then sums the org units); any other
//!   aggregation type is refused.
//! - An indicator's numerator and denominator are its expressions (see
//!   `expression`); its value is numerator × factor ÷ denominator, times
//!   days in the year ÷ days in the period when the indicator is
//!   annualised. No value when the numerator or denominator has none, or
//!   the denominator is 0.
//! - A data value counts towards a period when its own period lies wholly
//!   inside it (a week across two months counts towards neither, and is
//!   reported as `straddling_period`).
//! - A `--breakdown` must be a category: each category option combo counts
//!   towards the option of that category it holds. An item whose data
//!   elements do not all have the category gets no breakdown, as in
//!   analytics.
//!
//! The result is the same tables an analytics response would give, so the
//! reports, ids and strata are built by the same code as for analytics.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

use super::api::{Dhis2, in_filter};
use super::calendar::period_days;
use super::expression::{Expr, Item};
use super::measures::{Indicator, Kind};
use super::reports::{Analytics, AnalyticsRow, DateRange};
use crate::calendar::{days_in_year, iso};
use crate::error::{PipesError, Result};

/// Root org units in one `dataValueSets` request: 100 `orgUnit=` parameters
/// are about 2 KB of URL.
const ROOTS_PER_REQUEST: usize = 100;
use crate::fhir::ndjson::NdjsonReader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregation {
    Sum,
    AverageSumOrgUnit,
}

#[derive(Debug, Clone)]
pub struct Element {
    pub aggregation: Aggregation,
    /// Category option combo → its category options.
    pub cocs: HashMap<String, Vec<String>>,
    pub categories: Vec<String>,
}

/// A `--breakdown` category and its options' names.
#[derive(Debug, Clone)]
pub struct Category {
    pub uid: String,
    pub name: String,
    pub options: BTreeMap<String, String>,
}

/// What is computed in a batch: a period (code and inclusive day counts).
#[derive(Debug, Clone)]
pub struct Span {
    pub code: String,
    pub start: i64,
    pub end: i64,
}

impl Span {
    pub fn of_period(pe: &str) -> Option<Self> {
        let (start, end) = period_days(pe)?;
        Some(Self {
            code: pe.to_string(),
            start,
            end,
        })
    }

    pub fn of_range(r: &DateRange) -> Option<Self> {
        Some(Self {
            code: r.code(),
            start: crate::calendar::day_of(&r.start)?,
            end: crate::calendar::day_of(&r.end)?,
        })
    }
}

/// A batch computed: the totals table, one table per breakdown, and the
/// issues met as (kind, key, detail).
pub type Computed = (Analytics, Vec<Analytics>, Vec<(String, String, String)>);

#[derive(serde::Deserialize)]
struct DataValueSet {
    #[serde(default, rename = "dataValues")]
    data_values: Vec<DataValue>,
}

/// One data value, as `dataValueSets` gives it.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DataValue {
    #[serde(default)]
    data_element: String,
    #[serde(default)]
    period: String,
    #[serde(default)]
    org_unit: String,
    #[serde(default)]
    category_option_combo: String,
    #[serde(default)]
    attribute_option_combo: String,
    /// Text in DHIS2's JSON; a number is taken as well.
    #[serde(default)]
    value: Value,
    #[serde(default)]
    deleted: bool,
}

impl DataValue {
    fn value_text(&self) -> String {
        match &self.value {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }
}

/// Everything the computation needs, read once before the batches.
#[derive(Debug, Default)]
pub struct RawSource {
    pub elements: HashMap<String, Element>,
    pub constants: HashMap<String, f64>,
    /// Item id → (numerator, denominator); a data element is `#{de}` over 1.
    pub exprs: HashMap<String, (Expr, Expr)>,
    /// Org unit → each requested level at or above it, with the org unit
    /// at that level (itself included).
    pub up: HashMap<String, Vec<(u32, String)>>,
    /// Every org unit at the requested levels.
    pub level_units: Vec<String>,
    /// The org units data values are asked for under (`children=true`).
    pub roots: Vec<String>,
    pub categories: Vec<Category>,
}

fn usage(s: String) -> PipesError {
    PipesError::Usage(s)
}

/// The expressions, data elements, constants and categories of a run, or
/// a usage error naming what `--source data-values` cannot compute.
pub fn prepare(
    api: &Dhis2,
    indicators: &BTreeMap<String, Indicator>,
    breakdowns: &[String],
) -> Result<RawSource> {
    let mut src = RawSource::default();
    let mut items = Vec::new();
    let mut constants = Vec::new();
    for ind in indicators.values() {
        let pair = match ind.kind {
            Kind::ReportingRate => {
                return Err(usage(format!(
                    "{}: --source data-values does not compute reporting rates; leave out --data-set or use --source analytics",
                    ind.id
                )));
            }
            Kind::DataElement => (
                Expr::Item(Item {
                    de: ind.id.clone(),
                    coc: None,
                    aoc: None,
                }),
                Expr::Num(1.0),
            ),
            Kind::Indicator => {
                let parse = |side: &str, text: &str| {
                    Expr::parse(text).map_err(|what| {
                        usage(format!(
                            "indicator {} ({}): its {side} uses {what}, which --source data-values cannot compute; leave it out or use --source analytics",
                            ind.id, ind.name
                        ))
                    })
                };
                (
                    parse("numerator", &ind.numerator)?,
                    parse("denominator", &ind.denominator)?,
                )
            }
        };
        pair.0.items(&mut items);
        pair.1.items(&mut items);
        pair.0.constants(&mut constants);
        pair.1.constants(&mut constants);
        src.exprs.insert(ind.id.clone(), pair);
    }

    let des: BTreeSet<String> = items.into_iter().map(|i| i.de).collect();
    if !des.is_empty() {
        let list = api.get(
            "dataElements.json",
            &[
                ("paging", "false".into()),
                (
                    "fields",
                    "id,name,aggregationType,categoryCombo[categories[id],categoryOptionCombos[id,categoryOptions[id]]]".into(),
                ),
                ("filter", in_filter("id", &des)),
            ],
        )?;
        for v in list
            .get("dataElements")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = v.get("id").and_then(Value::as_str) else {
                continue;
            };
            let name = v.get("name").and_then(Value::as_str).unwrap_or(id);
            let aggregation = match v.get("aggregationType").and_then(Value::as_str) {
                Some("SUM") => Aggregation::Sum,
                Some("AVERAGE_SUM_ORG_UNIT") => Aggregation::AverageSumOrgUnit,
                other => {
                    return Err(usage(format!(
                        "data element {id} ({name}) aggregates as {}; --source data-values computes SUM and AVERAGE_SUM_ORG_UNIT only",
                        other.unwrap_or("nothing")
                    )));
                }
            };
            let ids = |v: Option<&Value>| -> Vec<String> {
                v.and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            };
            let combo = v.get("categoryCombo");
            let cocs = combo
                .and_then(|c| c.get("categoryOptionCombos"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|c| {
                    Some((
                        c.get("id")?.as_str()?.to_string(),
                        ids(c.get("categoryOptions")),
                    ))
                })
                .collect();
            src.elements.insert(
                id.to_string(),
                Element {
                    aggregation,
                    cocs,
                    categories: ids(combo.and_then(|c| c.get("categories"))),
                },
            );
        }
        let missing: Vec<&String> = des
            .iter()
            .filter(|d| !src.elements.contains_key(*d))
            .collect();
        if !missing.is_empty() {
            return Err(usage(format!(
                "the expressions refer to data elements DHIS2 does not have: {}",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }

    let constants: BTreeSet<String> = constants.into_iter().collect();
    if !constants.is_empty() {
        let list = api.get(
            "constants.json",
            &[
                ("paging", "false".into()),
                ("fields", "id,value".into()),
                ("filter", in_filter("id", &constants)),
            ],
        )?;
        for v in list
            .get("constants")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let (Some(id), Some(x)) = (
                v.get("id").and_then(Value::as_str),
                v.get("value").and_then(Value::as_f64),
            ) {
                src.constants.insert(id.to_string(), x);
            }
        }
        if let Some(c) = constants.iter().find(|c| !src.constants.contains_key(*c)) {
            return Err(usage(format!("DHIS2 has no constant {c}")));
        }
    }

    for b in breakdowns {
        let Some(v) = api.get_one("categories", b, "id,name,categoryOptions[id,name]")? else {
            return Err(usage(format!(
                "--breakdown {b}: with --source data-values a breakdown must be a category, and DHIS2 has no category {b}"
            )));
        };
        let options = v
            .get("categoryOptions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|o| {
                Some((
                    o.get("id")?.as_str()?.to_string(),
                    o.get("name")?.as_str()?.to_string(),
                ))
            })
            .collect();
        src.categories.push(Category {
            uid: b.clone(),
            name: v
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(b)
                .to_string(),
            options,
        });
    }
    Ok(src)
}

/// Org unit → parent org unit, from the `partOf` of the Locations that
/// carry an identifier under `system`.
pub fn load_parents(path: &Path, system: &str) -> Result<HashMap<String, Option<String>>> {
    let mut ou_of: HashMap<String, String> = HashMap::new();
    let mut parent_of: Vec<(String, Option<String>)> = Vec::new();
    for line in NdjsonReader::open(path)? {
        let line = line?;
        let v: Value = serde_json::from_str(&line.text).map_err(|e| {
            PipesError::Usage(format!("{}: line {}: {e}", path.display(), line.number))
        })?;
        if v.get("resourceType").and_then(Value::as_str) != Some("Location") {
            continue;
        }
        let Some(id) = v.get("id").and_then(Value::as_str) else {
            continue;
        };
        let uid = v
            .get("identifier")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|i| i.get("system").and_then(Value::as_str) == Some(system))
            .and_then(|i| i.get("value").and_then(Value::as_str));
        if let Some(uid) = uid {
            ou_of.insert(id.to_string(), uid.to_string());
            let parent = v
                .pointer("/partOf/reference")
                .and_then(Value::as_str)
                .and_then(|r| r.rsplit('/').next())
                .map(str::to_string);
            parent_of.push((uid.to_string(), parent));
        }
    }
    Ok(parent_of
        .into_iter()
        .map(|(uid, parent)| (uid, parent.and_then(|p| ou_of.get(&p).cloned())))
        .collect())
}

impl RawSource {
    /// Places the requested levels' org units in the hierarchy: `levels`
    /// pairs each level with its org units (as DHIS2 lists them).
    pub fn place(
        &mut self,
        parents: &HashMap<String, Option<String>>,
        levels: &[(u32, Vec<String>)],
        root: Option<&str>,
    ) {
        let at: HashMap<&str, u32> = levels
            .iter()
            .flat_map(|(l, ous)| ous.iter().map(move |o| (o.as_str(), *l)))
            .collect();
        for ou in parents.keys() {
            let mut chain = Vec::new();
            let mut cur = Some(ou.clone());
            // A partOf cycle ends the walk where it comes back round, so no
            // org unit is counted twice and the walk ends.
            let mut seen = HashSet::new();
            while let Some(c) = cur.filter(|c| seen.insert(c.clone())) {
                if let Some(&l) = at.get(c.as_str()) {
                    chain.push((l, c.clone()));
                }
                cur = parents.get(&c).cloned().flatten();
            }
            if !chain.is_empty() {
                self.up.insert(ou.clone(), chain);
            }
        }
        self.level_units = levels.iter().flat_map(|(_, o)| o.iter().cloned()).collect();
        self.roots = match root {
            Some(r) => vec![r.to_string()],
            None => levels
                .iter()
                .min_by_key(|(l, _)| *l)
                .map(|(_, o)| o.clone())
                .unwrap_or_default(),
        };
    }

    /// Data elements the given items' expressions use, sorted.
    pub fn elements_of(&self, items: &[String]) -> Vec<String> {
        let mut found = Vec::new();
        for id in items {
            if let Some((n, d)) = self.exprs.get(id) {
                n.items(&mut found);
                d.items(&mut found);
            }
        }
        let set: BTreeSet<String> = found.into_iter().map(|i| i.de).collect();
        set.into_iter().collect()
    }

    /// One `dataValueSets` query per chunk of root org units: without
    /// `--root` the roots are every org unit at the top requested level,
    /// which in one URL can pass a server's 8 KB limit. The roots are at one
    /// level, so their subtrees do not overlap and no value comes twice.
    fn queries(&self, des: &[String], start: i64, end: i64) -> Vec<Vec<(&'static str, String)>> {
        self.roots
            .chunks(ROOTS_PER_REQUEST)
            .map(|roots| {
                let mut q: Vec<(&str, String)> =
                    des.iter().map(|d| ("dataElement", d.clone())).collect();
                q.extend(roots.iter().map(|r| ("orgUnit", r.clone())));
                q.push(("children", "true".into()));
                q.push(("startDate", iso(start)));
                q.push(("endDate", iso(end)));
                q
            })
            .collect()
    }

    /// The `dataValues` of every query's response, each read straight into
    /// the fields used: a national response holds millions.
    fn data_values(
        api: &Dhis2,
        queries: Vec<Vec<(&'static str, String)>>,
    ) -> Result<Vec<DataValue>> {
        let mut out = Vec::new();
        for q in queries {
            let set: DataValueSet = api.get_as("dataValueSets.json", &q)?;
            out.extend(set.data_values);
        }
        Ok(out)
    }

    /// The targets (periods or ranges) any value of `des` changed in since
    /// `since`, deleted values included.
    pub fn changed(
        &self,
        api: &Dhis2,
        des: &[String],
        targets: &[Span],
        since: &str,
    ) -> Result<HashSet<String>> {
        let (Some(start), Some(end)) = (
            targets.iter().map(|t| t.start).min(),
            targets.iter().map(|t| t.end).max(),
        ) else {
            return Ok(HashSet::new());
        };
        let mut queries = self.queries(des, start, end);
        for q in &mut queries {
            q.push(("lastUpdated", since.to_string()));
            q.push(("includeDeleted", "true".into()));
        }
        let mut out = HashSet::new();
        for dv in &Self::data_values(api, queries)? {
            let Some((s, e)) = period_days(&dv.period) else {
                continue;
            };
            for t in targets {
                if t.start <= s && e <= t.end {
                    out.insert(t.code.clone());
                }
            }
        }
        Ok(out)
    }

    /// One batch: its data values fetched and computed into the tables an
    /// analytics response would give (totals, then one per breakdown), and
    /// the issues met on the way as (kind, key, detail).
    pub fn compute(&self, api: &Dhis2, items: &[&Indicator], targets: &[Span]) -> Result<Computed> {
        let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
        let des = self.elements_of(&ids);
        let mut issues = Vec::new();
        let start = targets.iter().map(|t| t.start).min().unwrap_or(0);
        let end = targets.iter().map(|t| t.end).max().unwrap_or(0);
        let values = Self::data_values(api, self.queries(&des, start, end))?;

        // (target, org unit, de, coc, aoc) → (sum, count)
        type Key = (usize, String, String, String, String);
        let mut raw: BTreeMap<Key, (f64, usize)> = BTreeMap::new();
        let mut noted: HashSet<String> = HashSet::new();
        let mut note = |kind: &str, key: &str, detail: String| {
            if noted.insert(format!("{kind}/{key}")) {
                issues.push((kind.to_string(), key.to_string(), detail));
            }
        };
        for dv in &values {
            let value = dv.value_text();
            if dv.deleted || !self.elements.contains_key(&dv.data_element) {
                continue;
            }
            let Some((ps, pe)) = period_days(&dv.period) else {
                note(
                    "unsupported_period",
                    &dv.period,
                    "a data value's period type has no dates here; value left out".into(),
                );
                continue;
            };
            let Some(t) = targets.iter().position(|t| t.start <= ps && pe <= t.end) else {
                if targets.iter().any(|t| ps <= t.end && t.start <= pe) {
                    note(
                        "straddling_period",
                        &dv.period,
                        "a data value's period runs across the edge of a reported period; left out of both".into(),
                    );
                }
                continue;
            };
            if !self.up.contains_key(&dv.org_unit) {
                note(
                    "unmapped_org_unit",
                    &dv.org_unit,
                    "no Location carries this org unit (or it is not under a reported level); its values are skipped".into(),
                );
                continue;
            }
            let Ok(x) = value.trim().parse::<f64>() else {
                note(
                    "non_numeric_value",
                    &format!("{}/{}/{}", dv.data_element, dv.org_unit, dv.period),
                    format!("value {:?} is not a number; left out", value),
                );
                continue;
            };
            let e = raw
                .entry((
                    t,
                    dv.org_unit.clone(),
                    dv.data_element.clone(),
                    dv.category_option_combo.clone(),
                    dv.attribute_option_combo.clone(),
                ))
                .or_insert((0.0, 0));
            e.0 += x;
            e.1 += 1;
        }

        // (target, level, org unit) → de → [(coc, aoc, value)]
        type Cell = HashMap<String, Vec<(String, String, f64)>>;
        let mut cells: BTreeMap<(usize, u32, String), Cell> = BTreeMap::new();
        for ((t, ou, de, coc, aoc), (sum, n)) in raw {
            let x = match self.elements[&de].aggregation {
                Aggregation::Sum => sum,
                Aggregation::AverageSumOrgUnit => sum / n as f64,
            };
            for (level, at) in &self.up[&ou] {
                let entries = cells
                    .entry((t, *level, at.clone()))
                    .or_default()
                    .entry(de.clone())
                    .or_default();
                match entries.iter_mut().find(|(c, a, _)| *c == coc && *a == aoc) {
                    Some(e) => e.2 += x,
                    None => entries.push((coc.clone(), aoc.clone(), x)),
                }
            }
        }

        let table = |rows: Vec<AnalyticsRow>| Analytics {
            rows,
            periods: targets
                .iter()
                .map(|t| (t.code.clone(), (iso(t.start), iso(t.end))))
                .collect(),
            names: self
                .categories
                .iter()
                .flat_map(|c| {
                    std::iter::once((c.uid.clone(), c.name.clone()))
                        .chain(c.options.iter().map(|(k, v)| (k.clone(), v.clone())))
                })
                .collect(),
            org_units: self.level_units.clone(),
            period_codes: targets.iter().map(|t| t.code.clone()).collect(),
        };
        let mut totals = Vec::new();
        let mut strata: Vec<Vec<AnalyticsRow>> = vec![Vec::new(); self.categories.len()];
        for ((t, _, ou), cell) in &cells {
            let target = &targets[*t];
            for ind in items {
                let (num, den) = &self.exprs[&ind.id];
                let row = |option: Option<(&Category, &String)>| {
                    let lookup = |item: &Item| -> Option<f64> {
                        let mut found = None;
                        for (coc, aoc, x) in cell.get(&item.de)? {
                            let keep = item.coc.as_ref().is_none_or(|c| c == coc)
                                && item.aoc.as_ref().is_none_or(|a| a == aoc)
                                && option.is_none_or(|(_, o)| {
                                    self.elements[&item.de]
                                        .cocs
                                        .get(coc)
                                        .is_some_and(|opts| opts.contains(o))
                                });
                            if keep {
                                *found.get_or_insert(0.0) += x;
                            }
                        }
                        found
                    };
                    self.row(
                        ind,
                        num,
                        den,
                        &lookup,
                        target,
                        ou,
                        option.map(|(_, o)| o.clone()),
                    )
                };
                if let Some(r) = row(None) {
                    totals.push(r);
                }
                for (b, cat) in self.categories.iter().enumerate() {
                    if !self.has_category(num, den, &cat.uid) {
                        continue;
                    }
                    for o in cat.options.keys() {
                        if let Some(r) = row(Some((cat, o))) {
                            strata[b].push(r);
                        }
                    }
                }
            }
        }
        let breakdowns = strata.into_iter().map(table).collect();
        Ok((table(totals), breakdowns, issues))
    }

    /// Every data element in the expressions has `category`.
    fn has_category(&self, num: &Expr, den: &Expr, category: &str) -> bool {
        let mut items = Vec::new();
        num.items(&mut items);
        den.items(&mut items);
        !items.is_empty()
            && items.iter().all(|i| {
                self.elements[&i.de]
                    .categories
                    .iter()
                    .any(|c| c == category)
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        &self,
        ind: &Indicator,
        num: &Expr,
        den: &Expr,
        lookup: &dyn Fn(&Item) -> Option<f64>,
        target: &Span,
        ou: &str,
        option: Option<String>,
    ) -> Option<AnalyticsRow> {
        let n = num.eval(lookup, &self.constants)?;
        let base = AnalyticsRow {
            dx: ind.id.clone(),
            pe: target.code.clone(),
            ou: ou.to_string(),
            option,
            ..Default::default()
        };
        if ind.kind == Kind::DataElement {
            return Some(AnalyticsRow {
                value: number(n),
                ..base
            });
        }
        let d = den.eval(lookup, &self.constants)?;
        if d == 0.0 {
            return None;
        }
        let days = (target.end - target.start + 1) as f64;
        let year = crate::calendar::civil_from_days(target.start).0;
        let annual = if ind.annualized {
            days_in_year(year) as f64 / days
        } else {
            1.0
        };
        Some(AnalyticsRow {
            value: number(n * ind.factor / d * annual),
            numerator: number(n),
            denominator: number(d),
            ..base
        })
    }
}

/// A computed number as analytics would write it: `50.0`, `0.45`.
fn number(x: f64) -> String {
    format!("{x:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_part_of_cycle_ends_the_walk_and_counts_each_unit_once() {
        let parents: HashMap<String, Option<String>> = [
            ("a", Some("b")),
            ("b", Some("a")),
            ("c", Some("d")),
            ("d", Some("c")),
        ]
        .into_iter()
        .map(|(o, p)| (o.to_string(), p.map(str::to_string)))
        .collect();
        let mut src = RawSource::default();
        // c is at the requested level; a and b's cycle has none.
        src.place(&parents, &[(2, vec!["c".into()])], None);
        assert_eq!(src.up.get("d"), Some(&vec![(2, "c".to_string())]));
        assert_eq!(src.up.get("c"), Some(&vec![(2, "c".to_string())]));
        assert!(!src.up.contains_key("a"));
    }

    #[test]
    fn many_roots_are_asked_for_in_chunks() {
        let src = RawSource {
            roots: (0..250).map(|i| format!("ou{i}")).collect(),
            ..Default::default()
        };
        let queries = src.queries(&["de".into()], 0, 30);
        assert_eq!(queries.len(), 3);
        let roots = |q: &Vec<(&str, String)>| q.iter().filter(|(k, _)| *k == "orgUnit").count();
        assert_eq!(
            queries.iter().map(roots).collect::<Vec<_>>(),
            [100, 100, 50]
        );
        assert!(
            queries
                .iter()
                .all(|q| q.contains(&("dataElement", "de".into())))
        );
    }

    #[test]
    fn numbers_read_like_analytics() {
        assert_eq!(number(50.0), "50.0");
        assert_eq!(number(0.45), "0.45");
    }
}
