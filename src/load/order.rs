//! Organizations (any non-Location) first, then Locations parents first: a
//! Location's managingOrganization must exist before the Location is
//! written, and a resource is loaded after the `partOf` parent that is
//! also in this set. References to resources not in the set count as
//! roots. Stable, so siblings keep their input order.

use std::collections::HashMap;

use serde_json::Value;

use crate::error::{PipesError, Result};
use crate::load::bundle::type_and_id;

/// The `partOf` parent as (type, id): the last two segments of
/// `partOf.reference`, without any `/_history/<v>`. A bare id, with no type
/// segment, is taken to be the resource's own type, which is what `partOf`
/// points at on Location and Organization.
pub fn parent_key(resource: &Value) -> Option<(&str, &str)> {
    let reference = resource.get("partOf")?.get("reference")?.as_str()?;
    let (t, id) = match type_and_id(reference) {
        Some(key) => key,
        None => (resource.get("resourceType")?.as_str()?, reference),
    };
    (!t.is_empty() && !id.is_empty()).then_some((t, id))
}

/// The depth of every node, given the index of each one's parent in the
/// set: 0 for a root, parent depth + 1 otherwise. A cycle is a usage
/// error naming one of its nodes by `label`.
pub fn depths(parents: &[Option<usize>], label: &dyn Fn(usize) -> String) -> Result<Vec<usize>> {
    let mut depths: Vec<Option<usize>> = vec![None; parents.len()];
    for start in 0..parents.len() {
        let mut chain: Vec<usize> = Vec::new();
        let mut current = Some(start);
        let mut base = 0usize;
        while let Some(i) = current {
            if let Some(d) = depths[i] {
                base = d + 1;
                break;
            }
            if chain.contains(&i) {
                return Err(PipesError::Usage(format!(
                    "partOf cycle detected involving {}; fix the hierarchy before loading",
                    label(i)
                )));
            }
            chain.push(i);
            current = parents[i];
        }
        for (offset, &i) in chain.iter().rev().enumerate() {
            depths[i] = Some(base + offset);
        }
    }
    Ok(depths
        .into_iter()
        .map(|d| d.expect("every node is set"))
        .collect())
}

pub fn order_parents_first(resources: Vec<Value>) -> Result<Vec<Value>> {
    // Keyed by type and id: a Location and an Organization may share an id.
    let by_key: HashMap<(&str, &str), usize> = resources
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let t = r.get("resourceType")?.as_str()?;
            let id = r.get("id")?.as_str()?;
            Some(((t, id), i))
        })
        .collect();
    let parents: Vec<Option<usize>> = resources
        .iter()
        .map(|r| parent_key(r).and_then(|p| by_key.get(&p).copied()))
        .collect();
    let label = |i: usize| {
        let r = &resources[i];
        format!(
            "{}/{}",
            r["resourceType"].as_str().unwrap_or("?"),
            r["id"].as_str().unwrap_or("?")
        )
    };
    let depths = depths(&parents, &label)?;
    let is_location =
        |i: usize| resources[i].get("resourceType").and_then(Value::as_str) == Some("Location");
    let mut keyed: Vec<(bool, usize, usize)> = (0..resources.len())
        .map(|i| (is_location(i), depths[i], i))
        .collect();
    keyed.sort_by_key(|&(l, d, i)| (l, d, i));
    let mut slots: Vec<Option<Value>> = resources.into_iter().map(Some).collect();
    Ok(keyed
        .into_iter()
        .map(|(_, _, i)| slots[i].take().expect("each index once"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn loc(id: &str, parent: Option<&str>) -> Value {
        let mut r = json!({"resourceType": "Location", "id": id});
        if let Some(p) = parent {
            r["partOf"] = json!({"reference": format!("Location/{p}")});
        }
        r
    }

    fn ids(v: &[Value]) -> Vec<&str> {
        v.iter().map(|r| r["id"].as_str().unwrap()).collect()
    }

    #[test]
    fn children_come_after_their_ancestors() {
        let input = vec![
            loc("clinic", Some("ward")),
            loc("ward", Some("state")),
            loc("state", None),
            loc("other", Some("state")),
        ];
        let out = order_parents_first(input).unwrap();
        assert_eq!(ids(&out), vec!["state", "ward", "other", "clinic"]);
    }

    #[test]
    fn external_parents_count_as_roots() {
        let input = vec![loc("b", Some("a")), loc("c", Some("not-here"))];
        let out = order_parents_first(input).unwrap();
        assert_eq!(ids(&out), vec!["b", "c"]);
    }

    #[test]
    fn a_cycle_is_a_usage_error() {
        let input = vec![loc("a", Some("b")), loc("b", Some("a"))];
        let err = order_parents_first(input).unwrap_err();
        assert!(
            matches!(&err, PipesError::Usage(m) if m.contains("cycle") && m.contains("Location/")),
            "{err}"
        );
    }

    #[test]
    fn organizations_come_before_every_location() {
        let input = vec![
            loc("clinic", Some("ward")),
            json!({"resourceType": "Organization", "id": "org-clinic"}),
            loc("ward", None),
        ];
        let out = order_parents_first(input).unwrap();
        assert_eq!(ids(&out), vec!["org-clinic", "ward", "clinic"]);
    }

    fn org(id: &str, parent: Option<&str>) -> Value {
        let mut r = json!({"resourceType": "Organization", "id": id});
        if let Some(p) = parent {
            r["partOf"] = json!({"reference": format!("Organization/{p}")});
        }
        r
    }

    fn keys(v: &[Value]) -> Vec<String> {
        v.iter()
            .map(|r| {
                format!(
                    "{}/{}",
                    r["resourceType"].as_str().unwrap(),
                    r["id"].as_str().unwrap()
                )
            })
            .collect()
    }

    #[test]
    fn the_same_id_on_two_types_is_not_a_cycle() {
        let input = vec![
            loc("b", None),
            org("a", None),
            loc("a", Some("b")),
            org("b", Some("a")),
        ];
        let out = order_parents_first(input).unwrap();
        assert_eq!(
            keys(&out),
            vec![
                "Organization/a",
                "Organization/b",
                "Location/b",
                "Location/a"
            ]
        );
    }

    #[test]
    fn a_parent_is_found_by_type_and_id_not_by_id_alone() {
        // Organization/x shares Location/x's id; the child still follows
        // Location/x, which is two levels below `top`.
        let input = vec![
            loc("child", Some("x")),
            loc("x", Some("top")),
            loc("top", None),
            org("x", None),
        ];
        let out = order_parents_first(input).unwrap();
        assert_eq!(
            keys(&out),
            vec![
                "Organization/x",
                "Location/top",
                "Location/x",
                "Location/child"
            ]
        );
    }

    #[test]
    fn a_versioned_or_absolute_parent_reference_is_followed() {
        let mut child = loc("child", None);
        child["partOf"] = json!({"reference": "Location/mid/_history/3"});
        let mut mid = loc("mid", None);
        mid["partOf"] = json!({"reference": "https://example.org/fhir/Location/top"});
        let out = order_parents_first(vec![child, mid, loc("top", None)]).unwrap();
        assert_eq!(ids(&out), vec!["top", "mid", "child"]);
    }

    #[test]
    fn resources_without_part_of_or_of_other_types_are_roots() {
        let input = vec![
            json!({"resourceType": "Organization", "id": "o"}),
            loc("a", Some("o")),
        ];
        let out = order_parents_first(input).unwrap();
        assert_eq!(ids(&out), vec!["o", "a"]);
    }
}
