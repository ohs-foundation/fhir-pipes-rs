//! The Organization half of a facility row: the columns mirrored from the
//! Location when they were edited, and the Organization's own.

use serde_json::Value;

use crate::diff::input::{ColumnValue, InputRow};
use crate::diff::rebuild::{apply_identifier_list, apply_profile_column, text_of};
use crate::fhir::Location;
use crate::fhir::edit::set_string;
use crate::fhir::organization::NewOrganization;
use crate::import::ORGANIZATION_KEYS;
use crate::profile::Profile;
use crate::report::Report;

/// True when the row carries `column` with a value different from what
/// the snapshot Location has. An unedited export repeats every column;
/// only a real edit may reach the Organization, so drift between the two
/// resources is never "corrected" by accident.
fn edited(row: &InputRow, column: &str, current: Option<&str>) -> bool {
    match row.columns.get(column) {
        None => false,
        Some(ColumnValue::Null) => current.is_some(),
        Some(ColumnValue::Text(t)) => Some(t.as_str()) != current,
        Some(_) => false,
    }
}

/// The Organization half of a facility row. `name`, `status` and the
/// profile's `mirror_to_organization` columns mirror from the Location when
/// edited (or always, for a create); the profile's Organization columns and
/// the whole `organization_identifier` list are Organization-only and apply
/// as given.
pub fn rebuild_organization(
    base: &Value,
    row: &InputRow,
    snapshot: Option<&Location>,
    profile: &Profile,
    report: &mut Report,
) -> Value {
    let mut obj = base.as_object().cloned().unwrap_or_default();
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>")
        .to_string();
    let mirror =
        |column: &str, current: Option<&str>| snapshot.is_none() || edited(row, column, current);
    if let Some(v) = row.columns.get("organization_identifier") {
        apply_identifier_list(&mut obj, v);
    }
    for (name, value) in &row.columns {
        let t = text_of(value);
        match name.as_str() {
            "name" if mirror("name", snapshot.and_then(|l| l.name.as_deref())) => {
                set_string(&mut obj, "name", t)
            }
            "status" if mirror("status", snapshot.and_then(|l| l.status.as_deref())) => {
                if let Some(s) = t {
                    obj.insert("active".into(), Value::Bool(s == "active"));
                }
            }
            _ => {}
        }
    }
    for (i, column) in profile.location.columns.iter().enumerate() {
        let Some(value) = row.columns.get(&column.name) else {
            continue;
        };
        let current = snapshot.and_then(|l| l.columns.get(i)?.as_deref());
        if column.mirror_to_organization && mirror(&column.name, current) {
            apply_profile_column(&mut obj, column, text_of(value), &id, report);
        }
    }
    for column in &profile.organization.columns {
        if let Some(value) = row.columns.get(&column.name) {
            apply_profile_column(&mut obj, column, text_of(value), &id, report);
        }
    }
    Value::Object(obj)
}

/// The Organization for a new facility row: the pairing shape `import location-sites --paired-org` writes,
/// then the row's columns on top.
pub fn new_organization(
    org_id: &str,
    row: &InputRow,
    profile: &Profile,
    report: &mut Report,
) -> Value {
    let base = NewOrganization {
        id: org_id,
        active: true,
        ..Default::default()
    }
    .into_json(&ORGANIZATION_KEYS);
    rebuild_organization(&base, row, None, profile, report)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::diff::input::Identifier;
    use crate::diff::rebuild::tests::{
        FACILITY_TYPE_SYSTEM, NHFR_CODE_SYSTEM, NHFR_UID_SYSTEM, icr, row, text,
    };
    use crate::fhir::organization::ORGANIZATION_TYPE_SYSTEM;

    fn org_base() -> Value {
        json!({"resourceType":"Organization","id":"org-a","active":true,"name":"A","meta":{"versionId":"9"},
            "identifier":[{"system":NHFR_CODE_SYSTEM,"value":"05/08/1"}],
            "type":[{"coding":[{"system":ORGANIZATION_TYPE_SYSTEM,"code":"prov"}]},
                    {"coding":[{"system":FACILITY_TYPE_SYSTEM,"code":"primary"}],"text":"Health Post"}]})
    }

    fn snapshot_location() -> Location {
        Location::parse(
            &json!({"resourceType":"Location","id":"a","name":"A","status":"active","managingOrganization":{"reference":"Organization/org-a"}}),
            &icr(),
            &mut Report::default(),
        )
        .unwrap()
    }

    #[test]
    fn mirrored_columns_reach_the_organization_only_when_edited() {
        let mut report = Report::default();
        let loc = snapshot_location();
        // Unedited name and status, and a facility level the Location never had: no change.
        let same = row(&[
            ("name", text("A")),
            ("status", text("active")),
            ("facility_level", ColumnValue::Null),
        ]);
        assert_eq!(
            rebuild_organization(&org_base(), &same, Some(&loc), &icr(), &mut report),
            org_base()
        );
        // Edited name and status.
        let edited = row(&[("name", text("B")), ("status", text("inactive"))]);
        let out = rebuild_organization(&org_base(), &edited, Some(&loc), &icr(), &mut report);
        assert_eq!(out["name"], "B");
        assert_eq!(out["active"], false);
        assert_eq!(out["meta"]["versionId"], "9");
        // Facility level edited on the row: the Organization's coding follows.
        let out = rebuild_organization(
            &org_base(),
            &row(&[("facility_level", text("secondary"))]),
            Some(&loc),
            &icr(),
            &mut report,
        );
        assert_eq!(out["type"][1]["coding"][0]["code"], "secondary");
        assert_eq!(
            out["type"][1]["text"], "Health Post",
            "the label is untouched"
        );
    }

    #[test]
    fn organization_only_columns_apply_directly() {
        let mut report = Report::default();
        let loc = snapshot_location();
        let r = row(&[
            ("nhfr_code", text("05/08/2")),
            ("nhfr_uid", text("999")),
            ("facility_level_text", text("Clinic")),
            ("ownership_text", text("Private")),
        ]);
        let out = rebuild_organization(&org_base(), &r, Some(&loc), &icr(), &mut report);
        assert_eq!(
            out["identifier"],
            json!([{"system":NHFR_CODE_SYSTEM,"value":"05/08/2"},{"system":NHFR_UID_SYSTEM,"value":"999"}])
        );
        assert_eq!(out["type"][1]["text"], "Clinic");
        assert_eq!(
            report.count("input_column_type"),
            1,
            "no ownership coding to label"
        );
        let out = rebuild_organization(
            &org_base(),
            &row(&[("facility_level_text", ColumnValue::Null)]),
            Some(&loc),
            &icr(),
            &mut report,
        );
        assert!(out["type"][1].get("text").is_none());
        let out = rebuild_organization(
            &org_base(),
            &row(&[(
                "organization_identifier",
                ColumnValue::Identifiers(vec![Identifier {
                    system: Some("s".into()),
                    value: Some("v".into()),
                }]),
            )]),
            Some(&loc),
            &icr(),
            &mut report,
        );
        assert_eq!(out["identifier"], json!([{"system":"s","value":"v"}]));
    }

    #[test]
    fn a_new_organization_carries_the_pairing_shape() {
        let mut report = Report::default();
        let r = row(&[
            ("name", text("New Site")),
            ("facility_level", text("primary")),
            ("facility_level_text", text("Health Post")),
            ("nhfr_code", text("05/09")),
        ]);
        let out = new_organization("org-new", &r, &icr(), &mut report);
        assert_eq!(out["resourceType"], "Organization");
        assert_eq!(out["id"], "org-new");
        assert_eq!(out["active"], true);
        assert_eq!(out["name"], "New Site");
        assert_eq!(out["type"][0]["coding"][0]["code"], "prov");
        assert_eq!(
            out["type"][1],
            json!({"coding":[{"system":FACILITY_TYPE_SYSTEM,"code":"primary"}],"text":"Health Post"})
        );
        assert_eq!(
            out["identifier"],
            json!([{"system":NHFR_CODE_SYSTEM,"value":"05/09"}])
        );
        assert!(out.get("meta").is_none());
    }
}
