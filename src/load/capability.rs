//! `PUT` to a new id needs update-as-create. One clear preflight error
//! beats hundreds of identical 404s -- but only when the server actually
//! says no. `rest.resource[].updateCreate` is optional in a
//! CapabilityStatement; HAPI FHIR, for one, supports update-as-create and
//! omits the element. An absent flag is unknown, not a refusal: fhir-pipes warns
//! and proceeds, and a create that really is unsupported fails on the first
//! bundle with the server's own error.

use std::collections::HashMap;

use serde_json::Value;

use crate::error::{PipesError, Result};

/// `rest.resource[].updateCreate` per resource type, for the types that
/// state it at all.
pub fn update_create_flags(capability: &Value) -> HashMap<String, bool> {
    let mut out = HashMap::new();
    for rest in capability
        .get("rest")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for resource in rest
            .get("resource")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let (Some(t), Some(flag)) = (
                resource.get("type").and_then(Value::as_str),
                resource.get("updateCreate").and_then(Value::as_bool),
            ) {
                out.insert(t.to_string(), flag);
            }
        }
    }
    out
}

/// `needed` are the types of resources without a `versionId`: those are
/// creates, and `PUT` to a new id needs update-as-create. Fails when a
/// needed type advertises `updateCreate: false`; returns the needed types
/// that do not state the flag, so the caller can warn and go ahead.
pub fn check_update_create(capability: &Value, needed: &[String]) -> Result<Vec<String>> {
    let flags = update_create_flags(capability);
    let mut needed: Vec<&str> = needed.iter().map(String::as_str).collect();
    needed.sort();
    needed.dedup();
    let refused: Vec<&str> = needed
        .iter()
        .copied()
        .filter(|t| flags.get(*t) == Some(&false))
        .collect();
    if !refused.is_empty() {
        return Err(PipesError::Usage(format!(
            "the input creates new {} resources but this FHIR server advertises updateCreate = false for them \
             (CapabilityStatement rest.resource.updateCreate); fhir-pipes load PUTs resources by id, which needs update-as-create. \
             On Google Cloud Healthcare API set enableUpdateCreate=true on the store.",
            refused.join(", ")
        )));
    }
    Ok(needed
        .into_iter()
        .filter(|t| !flags.contains_key(*t))
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn capability(types: &[(&str, Option<bool>)]) -> Value {
        json!({"resourceType": "CapabilityStatement", "rest": [{"resource":
            types.iter().map(|(t, u)| match u {
                Some(u) => json!({"type": t, "updateCreate": u}),
                None => json!({"type": t}),
            }).collect::<Vec<_>>()}]})
    }

    #[test]
    fn collects_the_flags_the_server_states() {
        let got = update_create_flags(&capability(&[
            ("Location", Some(true)),
            ("Organization", Some(false)),
            ("Group", None),
        ]));
        assert_eq!(got.get("Location"), Some(&true));
        assert_eq!(got.get("Organization"), Some(&false));
        assert_eq!(got.get("Group"), None);
        assert!(update_create_flags(&json!({"resourceType": "OperationOutcome"})).is_empty());
    }

    #[test]
    fn refuses_only_when_a_create_needs_a_type_that_says_false() {
        let cap = capability(&[("Location", Some(false))]);
        assert!(check_update_create(&cap, &[]).unwrap().is_empty());
        let err = check_update_create(&cap, &["Location".to_string()]).unwrap_err();
        assert!(
            matches!(&err, PipesError::Usage(m) if m.contains("updateCreate") && m.contains("Location")),
            "{err}"
        );
        let ok = check_update_create(
            &capability(&[("Location", Some(true))]),
            &["Location".to_string()],
        )
        .unwrap();
        assert!(ok.is_empty());
    }

    #[test]
    fn an_unstated_flag_is_reported_not_refused() {
        // HAPI FHIR: supports update-as-create, omits the element entirely.
        let cap = capability(&[("Location", None)]);
        let unstated =
            check_update_create(&cap, &["Location".to_string(), "Location".to_string()]).unwrap();
        assert_eq!(unstated, vec!["Location".to_string()]);
        // A type the statement does not list at all is unstated too.
        let unstated = check_update_create(
            &json!({"resourceType": "CapabilityStatement"}),
            &["Organization".to_string()],
        )
        .unwrap();
        assert_eq!(unstated, vec!["Organization".to_string()]);
    }
}
