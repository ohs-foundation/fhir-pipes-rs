//! One FHIR Organization, flattened into the fields fhir-pipes projects onto
//! the row of the Location that names it as managingOrganization: base FHIR,
//! plus the profile's Organization columns (ICR puts a facility's registry
//! codes and type labels there).

use serde_json::{Value, json};

use crate::fhir::location::{Identifier, non_empty, resource, str_field};
use crate::report::Report;

pub const ORGANIZATION_TYPE_SYSTEM: &str =
    "http://terminology.hl7.org/CodeSystem/organization-type";

/// The `prov` (Healthcare Provider) type every facility Organization has first.
pub fn provider_type() -> Value {
    json!({"coding": [{"system": ORGANIZATION_TYPE_SYSTEM, "code": "prov", "display": "Healthcare Provider"}]})
}

/// The top-level keys of a `NewOrganization` after `resourceType` and `id`,
/// in the order a writer puts them (see `LocationKey`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganizationKey {
    Meta,
    Identifier,
    Active,
    Type,
    Name,
}

/// A facility Organization fhir-pipes writes from scratch (`import
/// location-sites --paired-org`, `dhis2 locations`, a `diff` create): `prov`
/// first in its type. Empty lists and `None` are left out.
#[derive(Debug, Default)]
pub struct NewOrganization<'a> {
    pub id: &'a str,
    pub meta: Option<Value>,
    pub identifier: Vec<Value>,
    pub active: bool,
    /// Types after `prov`.
    pub extra_types: Vec<Value>,
    pub name: Option<&'a str>,
}

impl NewOrganization<'_> {
    /// The resource, its keys in `order`.
    pub fn into_json(self, order: &[OrganizationKey; 5]) -> Value {
        let types = std::iter::once(provider_type())
            .chain(self.extra_types)
            .collect();
        let fields = vec![
            (OrganizationKey::Meta, "meta", self.meta),
            (
                OrganizationKey::Identifier,
                "identifier",
                non_empty(self.identifier),
            ),
            (OrganizationKey::Active, "active", Some(json!(self.active))),
            (OrganizationKey::Type, "type", Some(Value::Array(types))),
            (OrganizationKey::Name, "name", self.name.map(|n| json!(n))),
        ];
        resource("Organization", self.id, fields, order)
    }
}

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

    #[test]
    fn a_new_organization_puts_prov_first_and_its_keys_in_the_given_order() {
        use OrganizationKey::*;
        let org = NewOrganization {
            id: "org-a",
            identifier: vec![crate::fhir::location::identifier("s", "v")],
            active: true,
            extra_types: vec![serde_json::json!({"text": "x"})],
            name: Some("A"),
            ..Default::default()
        };
        assert_eq!(
            org.into_json(&[Name, Type, Meta, Active, Identifier])
                .to_string(),
            r#"{"resourceType":"Organization","id":"org-a","name":"A","type":[{"coding":[{"system":"http://terminology.hl7.org/CodeSystem/organization-type","code":"prov","display":"Healthcare Provider"}]},{"text":"x"}],"active":true,"identifier":[{"system":"s","value":"v"}]}"#
        );
    }

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
