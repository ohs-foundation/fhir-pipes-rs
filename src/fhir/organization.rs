//! One FHIR Organization, flattened into the fields fhir-pipes projects onto
//! the row of the Location that names it as managingOrganization: base FHIR,
//! plus the profile's Organization columns (ICR puts a facility's registry
//! codes and type labels there).

use serde_json::Value;

use crate::fhir::location::{Identifier, str_field};
use crate::report::Report;

pub const ORGANIZATION_TYPE_SYSTEM: &str =
    "http://terminology.hl7.org/CodeSystem/organization-type";

#[derive(Debug, Clone, Default)]
pub struct Organization {
    pub id: String,
    pub version_id: Option<String>,
    pub last_updated: Option<String>,
    pub name: Option<String>,
    pub identifier: Vec<Identifier>,
    /// The profile's Organization columns, in profile order.
    pub columns: Vec<Option<String>>,
    /// The resource as JSON, key order preserved, untouched.
    pub fhir_json: String,
}

/// `text` of the first CodeableConcept (in a concept or a list of them)
/// that holds a coding under `system`.
pub fn concept_text_by_system(node: Option<&Value>, system: &str) -> Option<String> {
    let concepts: Vec<&Value> = match node? {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    concepts
        .into_iter()
        .find(|concept| {
            concept
                .get("coding")
                .and_then(Value::as_array)
                .is_some_and(|codings| {
                    codings
                        .iter()
                        .any(|c| c.get("system").and_then(Value::as_str) == Some(system))
                })
        })
        .and_then(|concept| concept.get("text")?.as_str().map(str::to_string))
}

impl Organization {
    /// Flatten one resource, with `profile`'s Organization columns. Returns
    /// None (after reporting) if it has no usable id or is not an Organization.
    pub fn parse(
        resource: &Value,
        profile: &crate::profile::Profile,
        report: &mut Report,
    ) -> Option<Organization> {
        let Value::Object(obj) = resource else {
            report.add(
                "malformed_field",
                "<unknown>",
                "Organization resource is not an object",
            );
            return None;
        };
        let id = match obj.get("id") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => {
                report.add("missing_id", "<unknown>", "Organization resource has no id");
                return None;
            }
        };
        if let Some(rt) = obj.get("resourceType").and_then(Value::as_str)
            && rt != "Organization"
        {
            report.add(
                "malformed_field",
                &id,
                &format!("resourceType is {rt}, not Organization"),
            );
            return None;
        }
        let mut org = Organization {
            id: id.clone(),
            name: str_field(obj, "name"),
            ..Default::default()
        };
        if let Some(Value::Object(meta)) = obj.get("meta") {
            org.version_id = str_field(meta, "versionId");
            org.last_updated = str_field(meta, "lastUpdated");
        }
        if let Some(Value::Array(items)) = obj.get("identifier") {
            for item in items {
                if let Value::Object(ident) = item {
                    org.identifier.push(Identifier {
                        system: str_field(ident, "system"),
                        value: str_field(ident, "value"),
                    });
                } else {
                    report.add("malformed_field", &id, "identifier entry is not an object");
                }
            }
        }
        org.columns = profile
            .organization
            .columns
            .iter()
            .map(|c| c.read(obj, &org.identifier))
            .collect();
        org.fhir_json = resource.to_string();
        Some(org)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORG: &str = r#"{"resourceType":"Organization","id":"org-1","active":true,"name":"Siyi Health Post",
        "meta":{"versionId":"7","lastUpdated":"2026-08-06T02:59:22Z"},
        "identifier":[{"system":"https://icr.healthcampaigns.org/identifiers/nga-nhfr-code","value":"05/08/1/1/1/0061"},
                      {"system":"https://icr.healthcampaigns.org/identifiers/nga-nhfr-uid","value":"21526030"}],
        "type":[{"coding":[{"system":"http://terminology.hl7.org/CodeSystem/organization-type","code":"prov"}]},
                {"coding":[{"system":"https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs","code":"primary"}],"text":"Health Post"},
                {"coding":[{"system":"https://icr.healthcampaigns.org/CodeSystem/icr-ownership-cs","code":"public"}],"text":"Local Government"}]}"#;

    #[test]
    fn parses_the_pairing_fields() {
        let mut report = Report::default();
        let profile = crate::profile::icr_example();
        let org = Organization::parse(&serde_json::from_str(ORG).unwrap(), &profile, &mut report)
            .unwrap();
        let col = |name: &str| {
            let i = profile
                .organization
                .columns
                .iter()
                .position(|c| c.name == name)
                .unwrap();
            org.columns[i].as_deref()
        };
        assert_eq!(org.id, "org-1");
        assert_eq!(org.version_id.as_deref(), Some("7"));
        assert_eq!(org.name.as_deref(), Some("Siyi Health Post"));
        assert_eq!(org.identifier.len(), 2);
        assert_eq!(col("nhfr_code"), Some("05/08/1/1/1/0061"));
        assert_eq!(col("nhfr_uid"), Some("21526030"));
        assert_eq!(col("facility_level_text"), Some("Health Post"));
        assert_eq!(col("ownership_text"), Some("Local Government"));
        assert!(
            org.fhir_json
                .starts_with("{\"resourceType\":\"Organization\"")
        );
        assert!(report.counts().is_empty());
    }

    #[test]
    fn missing_id_and_wrong_type_are_rejected() {
        let mut report = Report::default();
        assert!(
            Organization::parse(
                &serde_json::json!({"resourceType":"Organization"}),
                &crate::profile::Profile::base(),
                &mut report
            )
            .is_none()
        );
        assert_eq!(report.count("missing_id"), 1);
        assert!(
            Organization::parse(
                &serde_json::json!({"resourceType":"Location","id":"x"}),
                &crate::profile::Profile::base(),
                &mut report
            )
            .is_none()
        );
        assert_eq!(report.count("malformed_field"), 1);
    }

    #[test]
    fn text_is_read_from_the_concept_holding_the_system() {
        let v = serde_json::json!([{"coding":[{"system":"a","code":"1"}],"text":"A"},{"coding":[{"system":"b","code":"2"}]}]);
        assert_eq!(concept_text_by_system(Some(&v), "a").as_deref(), Some("A"));
        assert_eq!(concept_text_by_system(Some(&v), "b"), None);
        assert_eq!(concept_text_by_system(None, "a"), None);
    }
}
