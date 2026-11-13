//! `dhis2 list KIND`: the UIDs (or levels) that an option or a settings key
//! takes, read from the DHIS2 metadata API with GET requests only, every
//! page of them.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::api::Dhis2;
use crate::cli::{ListArgs, ListFormat, ListKind};
use fhir_pipes::error::{PipesError, Result};

/// One line of the list.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The UID, or for a level its number.
    pub id: String,
    pub name: String,
    /// Members of a group, org units at a level, …; `None` when the kind
    /// has none to count.
    pub count: Option<u64>,
}

/// How a kind is asked for and shown.
struct Kind {
    /// The metadata list: `{path}.json`, its objects under `path`.
    path: &'static str,
    /// The collection counted with DHIS2's `~size`, if any.
    count: Option<&'static str>,
    /// What the count column counts.
    counted: &'static str,
    /// Filters always applied.
    filters: &'static [&'static str],
    /// With `--group UID`: the filter on the members, `{}` for the UID.
    group: Option<&'static str>,
    /// The plural shown in the summary line.
    plural: &'static str,
    /// Which option and settings key takes the values.
    used_by: &'static str,
}

fn kind(k: ListKind) -> Kind {
    let base = Kind {
        path: "",
        count: None,
        counted: "",
        filters: &[],
        group: None,
        plural: "",
        used_by: "",
    };
    match k {
        ListKind::IndicatorGroups => Kind {
            path: "indicatorGroups",
            count: Some("indicators"),
            counted: "INDICATORS",
            plural: "indicator groups",
            used_by: "--indicator-group or [indicators] indicator_groups",
            ..base
        },
        ListKind::Indicators => Kind {
            path: "indicators",
            group: Some("indicatorGroups.id:eq:{}"),
            plural: "indicators",
            used_by: "--indicator or [indicators] indicators",
            ..base
        },
        ListKind::DataElementGroups => Kind {
            path: "dataElementGroups",
            count: Some("dataElements"),
            counted: "DATA ELEMENTS",
            plural: "data element groups",
            used_by: "--data-element-group or [indicators] data_element_groups",
            ..base
        },
        ListKind::DataElementGroupSets => Kind {
            path: "dataElementGroupSets",
            count: Some("dataElementGroups"),
            counted: "GROUPS",
            plural: "data element group sets",
            used_by: "--data-element-group-set or [indicators] data_element_group_sets, \
                      and --breakdown or [reports] breakdowns",
            ..base
        },
        ListKind::DataElements => Kind {
            path: "dataElements",
            filters: &["domainType:eq:AGGREGATE"],
            group: Some("dataElementGroups.id:eq:{}"),
            plural: "aggregate data elements",
            used_by: "--data-element or [indicators] data_elements",
            ..base
        },
        ListKind::DataSets => Kind {
            path: "dataSets",
            count: Some("dataSetElements"),
            counted: "DATA ELEMENTS",
            plural: "data sets",
            used_by: "--data-set or [indicators] data_sets",
            ..base
        },
        ListKind::OrgUnitLevels => Kind {
            path: "organisationUnitLevels",
            counted: "ORG UNITS",
            plural: "org unit levels",
            used_by: "--level or [reports] levels, and --facility-level or [locations] \
                      facility_level",
            ..base
        },
        ListKind::OrgUnitGroupSets => Kind {
            path: "organisationUnitGroupSets",
            count: Some("organisationUnitGroups"),
            counted: "GROUPS",
            plural: "org unit group sets",
            used_by: "--group-set or [locations] group_sets, and --breakdown or [reports] \
                      breakdowns",
            ..base
        },
        ListKind::OrgUnitGroups => Kind {
            path: "organisationUnitGroups",
            count: Some("organisationUnits"),
            counted: "ORG UNITS",
            group: Some("groupSets.id:eq:{}"),
            plural: "org unit groups",
            used_by: "--org-unit-group or [locations] org_unit_groups",
            ..base
        },
        ListKind::OrgUnits => Kind {
            path: "organisationUnits",
            count: Some("children"),
            counted: "CHILDREN",
            plural: "org units",
            used_by: "--root or [locations] root and [reports] root",
            ..base
        },
        ListKind::Categories => Kind {
            path: "categories",
            count: Some("categoryOptions"),
            counted: "OPTIONS",
            filters: &["dataDimension:eq:true"],
            plural: "categories",
            used_by: "--breakdown or [reports] breakdowns",
            ..base
        },
    }
}

pub fn run_list(args: &ListArgs) -> Result<()> {
    let k = kind(args.kind);
    if args.group.is_some() && k.group.is_none() {
        return Err(PipesError::Usage(
            "--group is for indicators, data-elements and org-unit-groups".into(),
        ));
    }
    if args.level.is_some() && args.kind != ListKind::OrgUnits {
        return Err(PipesError::Usage("--level is for org-units".into()));
    }
    let api = Dhis2::connect(&args.conn)?;
    let rows = if args.kind == ListKind::OrgUnitLevels {
        levels(&api, args.filter.as_deref())?
    } else {
        rows(&api, &k, args)?
    };
    let mut out = std::io::stdout().lock();
    match args.format {
        ListFormat::Json => {
            let list: Vec<Value> = rows
                .iter()
                .map(|r| {
                    let mut v = if args.kind == ListKind::OrgUnitLevels {
                        json!({"level": r.id.parse::<u64>().ok(), "name": r.name})
                    } else {
                        json!({"id": r.id, "name": r.name})
                    };
                    if let Some(n) = r.count {
                        v["count"] = json!(n);
                    }
                    v
                })
                .collect();
            let text = serde_json::to_string_pretty(&list)
                .map_err(|e| PipesError::Usage(e.to_string()))?;
            write_out(&mut out, &format!("{text}\n"))
        }
        ListFormat::Table => {
            let first = if args.kind == ListKind::OrgUnitLevels {
                "LEVEL"
            } else {
                "UID"
            };
            let counted =
                (args.kind == ListKind::OrgUnitLevels || k.count.is_some()).then_some(k.counted);
            write_out(&mut out, &table(first, counted, &rows))?;
            write_out(
                &mut out,
                &format!("\n{} {}; use with {}\n", rows.len(), k.plural, k.used_by),
            )
        }
    }
}

fn write_out(out: &mut impl std::io::Write, text: &str) -> Result<()> {
    out.write_all(text.as_bytes())
        .map_err(|e| PipesError::io("stdout", e))
}

/// Sorted by name, ignoring case, then by id, so the order is stable.
fn sorted(mut rows: Vec<Row>) -> Vec<Row> {
    rows.sort_by(|a, b| {
        (a.name.to_lowercase(), &a.name, &a.id).cmp(&(b.name.to_lowercase(), &b.name, &b.id))
    });
    rows
}

/// Every object of the kind, page by page.
fn rows(api: &Dhis2, k: &Kind, args: &ListArgs) -> Result<Vec<Row>> {
    let mut fields = "id,name".to_string();
    if let Some(c) = k.count {
        fields.push_str(&format!(",{c}~size"));
    }
    let mut query = vec![("fields", fields)];
    for f in k.filters {
        query.push(("filter", f.to_string()));
    }
    if let (Some(uid), Some(f)) = (&args.group, k.group) {
        query.push(("filter", f.replace("{}", uid)));
    }
    if let Some(level) = args.level {
        query.push(("filter", format!("level:eq:{level}")));
    }
    if let Some(text) = &args.filter {
        query.push(("filter", format!("name:ilike:{text}")));
    }
    let list = api.paged(&format!("{}.json", k.path), k.path, &query)?;
    Ok(sorted(
        list.iter()
            .filter_map(|v| {
                Some(Row {
                    id: v.get("id")?.as_str()?.to_string(),
                    name: v
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    count: k.count.and_then(|c| v.get(c)).and_then(Value::as_u64),
                })
            })
            .collect(),
    ))
}

/// The org unit levels, each with how many org units it has. `filter`
/// matches the name as `ilike` does.
fn levels(api: &Dhis2, filter: Option<&str>) -> Result<Vec<Row>> {
    let v = api.get(
        "organisationUnitLevels.json",
        &[("paging", "false".into()), ("fields", "level,name".into())],
    )?;
    let mut by_level = BTreeMap::new();
    for l in v
        .get("organisationUnitLevels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(level), Some(name)) = (
            l.get("level").and_then(Value::as_u64),
            l.get("name").and_then(Value::as_str),
        ) else {
            continue;
        };
        if filter.is_some_and(|f| !name.to_lowercase().contains(&f.to_lowercase())) {
            continue;
        }
        by_level.insert(level, name.to_string());
    }
    let mut rows = Vec::new();
    for (level, name) in by_level {
        let count = crate::reports::org_unit_count(api, level as u32, None)?;
        rows.push(Row {
            id: level.to_string(),
            name,
            count: Some(count),
        });
    }
    // Levels in their order, top first: their names do not sort usefully.
    Ok(rows)
}

/// The rows as columns: `first` (UID or LEVEL), NAME, and the count.
fn table(first: &str, counted: Option<&str>, rows: &[Row]) -> String {
    let w = rows
        .iter()
        .map(|r| r.id.len())
        .chain([first.len()])
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    let line = |id: &str, name: &str, count: Option<String>| match count {
        Some(n) => format!("{id:<w$}  {n:>9}  {name}"),
        None => format!("{id:<w$}  {name}"),
    };
    lines.push(line(first, "NAME", counted.map(|c| c.to_string())));
    for r in rows {
        lines.push(line(
            &r.id,
            &r.name,
            counted.map(|_| r.count.map_or("?".to_string(), |n| n.to_string())),
        ));
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, name: &str, count: Option<u64>) -> Row {
        Row {
            id: id.into(),
            name: name.into(),
            count,
        }
    }

    #[test]
    fn rows_sort_by_name_ignoring_case_then_by_uid() {
        let rows = sorted(vec![
            row("b", "measles", None),
            row("c", "ANC", None),
            row("a", "measles", None),
            row("d", "Malaria", None),
        ]);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["c", "d", "a", "b"]);
    }

    #[test]
    fn the_table_has_a_column_per_field() {
        let t = table(
            "UID",
            Some("INDICATORS"),
            &[row("srxUNt2w5jn", "Immunization", Some(12))],
        );
        assert_eq!(
            t,
            "UID          INDICATORS  NAME\nsrxUNt2w5jn         12  Immunization\n"
        );
        let t = table("UID", None, &[row("tUIlpyeeX9N", "Penta 3", None)]);
        assert_eq!(t, "UID          NAME\ntUIlpyeeX9N  Penta 3\n");
    }

    #[test]
    fn every_kind_names_the_option_that_takes_it() {
        use clap::ValueEnum;
        for k in ListKind::value_variants() {
            let k = kind(*k);
            assert!(
                !k.path.is_empty() && k.used_by.starts_with("--"),
                "{}",
                k.path
            );
        }
    }
}
