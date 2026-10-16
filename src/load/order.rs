//! Organizations (any non-Location) first, then Locations parents first: a
//! Location's managingOrganization must exist before the Location is
//! written, and a resource is loaded after the `partOf` parent that is
//! also in this set. References to resources not in the set count as
//! roots. Stable, so siblings keep their input order.

use std::collections::HashMap;

use serde_json::Value;

use crate::error::{PipesError, Result};

/// The id at the end of `partOf.reference`, if any.
pub fn parent_id(resource: &Value) -> Option<&str> {
    let reference = resource.get("partOf")?.get("reference")?.as_str()?;
    let last = reference.rsplit('/').next().unwrap_or(reference);
    (!last.is_empty()).then_some(last)
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
    let by_id: HashMap<&str, usize> = resources
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.get("id")?.as_str().map(|id| (id, i)))
        .collect();
    let parents: Vec<Option<usize>> = resources
        .iter()
        .map(|r| parent_id(r).and_then(|p| by_id.get(p).copied()))
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
