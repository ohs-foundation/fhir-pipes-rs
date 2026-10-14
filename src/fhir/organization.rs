//! One FHIR Organization, flattened into the fields fhir-pipes projects onto its
//! facility's row. The ICR facility pairing (`ICRFacilityOrganization`)
//! puts the registry codes and the human-readable type labels here.

use serde_json::Value;

use crate::fhir::location::{
    FACILITY_TYPE_SYSTEM, Identifier, OWNERSHIP_SYSTEM, coding_code_by_system, str_field,
};
use crate::report::Report;

pub const NHFR_CODE_SYSTEM: &str = "https://icr.healthcampaigns.org/identifiers/nga-nhfr-code";
pub const NHFR_UID_SYSTEM: &str = "https://icr.healthcampaigns.org/identifiers/nga-nhfr-uid";
pub const ORGANIZATION_TYPE_SYSTEM: &str =
    "http://terminology.hl7.org/CodeSystem/organization-type";

#[derive(Debug, Clone, Default)]
pub struct Organization {
    pub id: String,
    pub version_id: Option<String>,
    pub last_updated: Option<String>,
    pub name: Option<String>,
    pub identifier: Vec<Identifier>,
    pub nhfr_code: Option<String>,
    pub nhfr_uid: Option<String>,
    pub facility_level: Option<String>,
    pub ownership: Option<String>,
    pub facility_level_text: Option<String>,
    pub ownership_text: Option<String>,
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
    /// Flatten one resource. Returns None (after reporting) if it has no
    /// usable id or is not an Organization.
    pub fn parse(resource: &Value, report: &mut Report) -> Option<Organization> {
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
        let by_system = |system: &str| {
            org.identifier
                .iter()
                .find(|i| i.system.as_deref() == Some(system))
                .and_then(|i| i.value.clone())
        };
        org.nhfr_code = by_system(NHFR_CODE_SYSTEM);
        org.nhfr_uid = by_system(NHFR_UID_SYSTEM);
        org.facility_level = coding_code_by_system(obj.get("type"), FACILITY_TYPE_SYSTEM);
        org.ownership = coding_code_by_system(obj.get("type"), OWNERSHIP_SYSTEM);
        org.facility_level_text = concept_text_by_system(obj.get("type"), FACILITY_TYPE_SYSTEM);
        org.ownership_text = concept_text_by_system(obj.get("type"), OWNERSHIP_SYSTEM);
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
        let org = Organization::parse(&serde_json::from_str(ORG).unwrap(), &mut report).unwrap();
        assert_eq!(org.id, "org-1");
        assert_eq!(org.version_id.as_deref(), Some("7"));
        assert_eq!(org.name.as_deref(), Some("Siyi Health Post"));
        assert_eq!(org.identifier.len(), 2);
        assert_eq!(org.nhfr_code.as_deref(), Some("05/08/1/1/1/0061"));
        assert_eq!(org.nhfr_uid.as_deref(), Some("21526030"));
        assert_eq!(org.facility_level.as_deref(), Some("primary"));
        assert_eq!(org.ownership.as_deref(), Some("public"));
        assert_eq!(org.facility_level_text.as_deref(), Some("Health Post"));
        assert_eq!(org.ownership_text.as_deref(), Some("Local Government"));
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
                &mut report
            )
            .is_none()
        );
        assert_eq!(report.count("missing_id"), 1);
        assert!(
            Organization::parse(
                &serde_json::json!({"resourceType":"Location","id":"x"}),
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
