//! Transaction bundles of `PUT <Type>/<id>`, `ifMatch` on every entry that
//! has a version to check, and of `DELETE <Type>/<id>` (`sync --delete`).

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

pub struct Entry {
    pub resource: Value,
    pub resource_type: String,
    pub id: String,
    /// `meta.versionId` from the snapshot; `None` is a create.
    pub version: Option<String>,
}

pub struct Bundle {
    pub entries: Vec<Entry>,
    /// `(type, id)` of each resource to delete, after the PUTs.
    pub deletes: Vec<(String, String)>,
}

pub fn if_match(version: &str) -> String {
    format!("W/\"{version}\"")
}

/// A transaction Bundle as it is posted, borrowing the resources instead
/// of copying them into a new `Value`. The fields are in this order, so
/// the bytes are those of
/// `{"resourceType":"Bundle","type":"transaction","entry":[…]}`.
#[derive(Serialize)]
struct Body<'a> {
    #[serde(rename = "resourceType")]
    resource_type: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
    entry: Vec<BodyEntry<'a>>,
}

#[derive(Serialize)]
struct BodyEntry<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    resource: Option<&'a Value>,
    request: Request,
}

#[derive(Serialize)]
struct Request {
    method: &'static str,
    url: String,
    #[serde(rename = "ifMatch", skip_serializing_if = "Option::is_none")]
    if_match: Option<String>,
}

impl Bundle {
    /// The transaction Bundle as compact JSON: a `PUT` entry per resource,
    /// with `ifMatch` when it has a version, then a `DELETE` entry per
    /// delete.
    pub fn to_bytes(&self) -> Vec<u8> {
        let puts = self.entries.iter().map(|e| BodyEntry {
            resource: Some(&e.resource),
            request: Request {
                method: "PUT",
                url: format!("{}/{}", e.resource_type, e.id),
                if_match: e.version.as_deref().map(if_match),
            },
        });
        let deletes = self.deletes.iter().map(|(t, id)| BodyEntry {
            resource: None,
            request: Request {
                method: "DELETE",
                url: format!("{t}/{id}"),
                if_match: None,
            },
        });
        let body = Body {
            resource_type: "Bundle",
            kind: "transaction",
            entry: puts.chain(deletes).collect(),
        };
        serde_json::to_vec(&body).expect("a Bundle of JSON values serialises")
    }

    /// Entries of either kind.
    pub fn size(&self) -> usize {
        self.entries.len() + self.deletes.len()
    }

    /// One line for `--dry-run`: `Location/a update@3, Location/b create`.
    pub fn describe(&self) -> String {
        self.entries
            .iter()
            .map(|e| match &e.version {
                Some(v) => format!("{}/{} update@{v}", e.resource_type, e.id),
                None => format!("{}/{} create", e.resource_type, e.id),
            })
            .chain(
                self.deletes
                    .iter()
                    .map(|(t, id)| format!("{t}/{id} delete")),
            )
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The type and id a reference string points at: its last two segments,
/// after any `/_history/<v>` is cut off. `None` without a type segment.
pub fn type_and_id(reference: &str) -> Option<(&str, &str)> {
    let r = reference.split("/_history/").next().unwrap_or(reference);
    let mut parts = r.rsplit('/');
    match (parts.next(), parts.next()) {
        (Some(id), Some(t)) => Some((t, id)),
        _ => None,
    }
}

/// Every `reference` string in a resource, as `Type/id` (see `type_and_id`).
fn references(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                match (k.as_str(), x) {
                    ("reference", Value::String(r)) => {
                        if let Some((t, id)) = type_and_id(r) {
                            out.push(format!("{t}/{id}"));
                        }
                    }
                    _ => references(x, out),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| references(x, out)),
        _ => {}
    }
}

/// For each bundle, the earlier bundles holding a resource it references
/// (a `partOf` parent, a `managingOrganization`, …). Loaded one at a time
/// those are always committed first; in parallel, a bundle waits for them.
/// Later bundles are never waited for, so there is no cycle to wait on.
pub fn dependencies(bundles: &[Bundle]) -> Vec<Vec<usize>> {
    let mut home: HashMap<String, usize> = HashMap::new();
    for (i, b) in bundles.iter().enumerate() {
        for e in &b.entries {
            home.insert(format!("{}/{}", e.resource_type, e.id), i);
        }
    }
    bundles
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let mut refs = Vec::new();
            for e in &b.entries {
                references(&e.resource, &mut refs);
            }
            let mut deps: Vec<usize> = refs
                .iter()
                .filter_map(|r| home.get(r).copied())
                .filter(|&j| j < i)
                .collect();
            deps.sort_unstable();
            deps.dedup();
            deps
        })
        .collect()
}

/// Chunk already-ordered resources into bundles of `batch_size`. Every
/// resource has been validated to carry string `resourceType` and `id`.
pub fn plan(resources: Vec<Value>, batch_size: usize) -> Vec<Bundle> {
    let mut bundles = Vec::new();
    let mut current: Vec<Entry> = Vec::new();
    for resource in resources {
        let entry = Entry {
            resource_type: resource["resourceType"].as_str().unwrap_or("").to_string(),
            id: resource["id"].as_str().unwrap_or("").to_string(),
            version: resource
                .get("meta")
                .and_then(|m| m.get("versionId"))
                .and_then(Value::as_str)
                .map(str::to_string),
            resource,
        };
        current.push(entry);
        if current.len() == batch_size.max(1) {
            bundles.push(Bundle {
                entries: std::mem::take(&mut current),
                deletes: Vec::new(),
            });
        }
    }
    if !current.is_empty() {
        bundles.push(Bundle {
            entries: current,
            deletes: Vec::new(),
        });
    }
    bundles
}

/// `DELETE <resource_type>/<id>` for each id, `batch_size` to a bundle.
pub fn plan_deletes(resource_type: &str, ids: &[String], batch_size: usize) -> Vec<Bundle> {
    ids.chunks(batch_size.max(1))
        .map(|chunk| Bundle {
            entries: Vec::new(),
            deletes: chunk
                .iter()
                .map(|id| (resource_type.to_string(), id.clone()))
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn json_of(b: &Bundle) -> Value {
        serde_json::from_slice(&b.to_bytes()).unwrap()
    }

    /// How bundles were built before `to_bytes`: a new `Value` holding a
    /// copy of every resource.
    fn copied(b: &Bundle) -> Vec<u8> {
        let entries: Vec<Value> = b
            .entries
            .iter()
            .map(|e| {
                let mut request =
                    json!({"method": "PUT", "url": format!("{}/{}", e.resource_type, e.id)});
                if let Some(v) = &e.version {
                    request["ifMatch"] = Value::String(if_match(v));
                }
                json!({"resource": e.resource, "request": request})
            })
            .chain(b.deletes.iter().map(
                |(t, id)| json!({"request": {"method": "DELETE", "url": format!("{t}/{id}")}}),
            ))
            .collect();
        json!({"resourceType": "Bundle", "type": "transaction", "entry": entries})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn the_borrowed_bundle_has_the_bytes_of_the_copied_one() {
        let resources = vec![
            json!({"resourceType": "Location", "id": "a", "meta": {"versionId": "1", "lastUpdated": "2026-01-01T00:00:00.123+00:00"},
                   "name": "Kénéma \"east\"\n\u{1F600}", "position": {"latitude": 8.1e-7, "longitude": -11.25}}),
            json!({"resourceType": "Location", "id": "b", "partOf": {"reference": "Location/a"}, "z": null, "a": [1, 2.5, true]}),
            json!({"resourceType": "MeasureReport", "id": "m.1-x", "meta": {"versionId": "W/\"q\""}, "group": [{"measureScore": {"value": 100000000000000000000.0}}]}),
        ];
        for size in [1, 2, 3] {
            for b in plan(resources.clone(), size) {
                assert_eq!(
                    String::from_utf8(b.to_bytes()).unwrap(),
                    String::from_utf8(copied(&b)).unwrap()
                );
            }
        }
        let mut mixed = plan(resources, 3).remove(0);
        mixed.deletes = vec![
            ("Patient".into(), "p1".into()),
            ("Patient".into(), "p2".into()),
        ];
        assert_eq!(mixed.to_bytes(), copied(&mixed));
        let deletes = plan_deletes("Patient", &["x".to_string()], 5).remove(0);
        assert_eq!(deletes.to_bytes(), copied(&deletes));
        let empty = Bundle {
            entries: Vec::new(),
            deletes: Vec::new(),
        };
        assert_eq!(empty.to_bytes(), copied(&empty));
    }

    /// The ICR MeasureReports, when present: every bundle of 500 is the
    /// same bytes both ways.
    #[test]
    #[ignore = "needs FHIR_PIPES_BENCH_NDJSON"]
    fn the_borrowed_bundles_of_a_real_file_have_the_same_bytes() {
        let Ok(path) = std::env::var("FHIR_PIPES_BENCH_NDJSON") else {
            return;
        };
        let text = std::fs::read_to_string(path).unwrap();
        let resources: Vec<Value> = text
            .lines()
            .take(20_000)
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let bundles = plan(resources, 500);
        let started = std::time::Instant::now();
        let copied_bytes: Vec<Vec<u8>> = bundles.iter().map(copied).collect();
        let copied_time = started.elapsed();
        let started = std::time::Instant::now();
        let borrowed: Vec<Vec<u8>> = bundles.iter().map(Bundle::to_bytes).collect();
        let borrowed_time = started.elapsed();
        eprintln!(
            "{} bundles: copied {copied_time:?}, borrowed {borrowed_time:?}",
            bundles.len()
        );
        assert_eq!(copied_bytes, borrowed);
    }

    #[test]
    fn a_bundle_depends_on_earlier_bundles_it_references() {
        let resources = vec![
            json!({"resourceType": "Organization", "id": "o"}),
            json!({"resourceType": "Location", "id": "a"}),
            json!({"resourceType": "Location", "id": "b", "partOf": {"reference": "Location/a"}}),
            json!({"resourceType": "Location", "id": "c", "partOf": {"reference": "https://x/fhir/Location/b/_history/2"},
                   "managingOrganization": {"reference": "Organization/o"}}),
            // A reference to a later bundle or outside the input is not waited for.
            json!({"resourceType": "Location", "id": "d", "partOf": {"reference": "Location/zzz"}}),
        ];
        let bundles = plan(resources, 1);
        assert_eq!(
            dependencies(&bundles),
            [vec![], vec![], vec![1], vec![0, 2], vec![]]
        );
        let two = plan(
            vec![
                json!({"resourceType": "Location", "id": "a"}),
                json!({"resourceType": "Location", "id": "b", "partOf": {"reference": "Location/a"}}),
            ],
            2,
        );
        // Within one bundle the transaction itself takes care of it.
        assert_eq!(dependencies(&two), [Vec::<usize>::new()]);
    }

    #[test]
    fn deletes_are_entries_without_a_resource() {
        let ids = ["a".to_string(), "b".to_string(), "c".to_string()];
        let bundles = plan_deletes("Patient", &ids, 2);
        assert_eq!(bundles.len(), 2);
        assert_eq!(bundles[0].size(), 2);
        assert_eq!(
            json_of(&bundles[0])["entry"],
            json!([
                {"request": {"method": "DELETE", "url": "Patient/a"}},
                {"request": {"method": "DELETE", "url": "Patient/b"}},
            ])
        );
        assert_eq!(bundles[1].describe(), "Patient/c delete");
        assert!(dependencies(&bundles).iter().all(Vec::is_empty));
    }

    #[test]
    fn if_match_is_a_weak_etag() {
        assert_eq!(if_match("3"), "W/\"3\"");
    }

    #[test]
    fn entries_are_chunked_in_order_with_if_match_only_for_existing_resources() {
        let resources = vec![
            json!({"resourceType": "Location", "id": "a", "meta": {"versionId": "1"}}),
            json!({"resourceType": "Location", "id": "b"}),
            json!({"resourceType": "Organization", "id": "o", "meta": {"versionId": "7"}}),
        ];
        let bundles = plan(resources, 2);
        assert_eq!(bundles.len(), 2);
        assert_eq!(bundles[0].entries.len(), 2);
        assert_eq!(bundles[1].entries.len(), 1);
        let json = json_of(&bundles[0]);
        assert_eq!(json["resourceType"], "Bundle");
        assert_eq!(json["type"], "transaction");
        assert_eq!(
            json["entry"][0]["request"],
            json!({"method": "PUT", "url": "Location/a", "ifMatch": "W/\"1\""})
        );
        assert_eq!(
            json["entry"][1]["request"],
            json!({"method": "PUT", "url": "Location/b"})
        );
        assert_eq!(json["entry"][0]["resource"]["id"], "a");
        assert_eq!(
            json_of(&bundles[1])["entry"][0]["request"]["url"],
            "Organization/o"
        );
        assert_eq!(
            bundles[0].describe(),
            "Location/a update@1, Location/b create"
        );
    }
}
