//! Edits to FHIR JSON in place: strings, references, identifiers by system,
//! codings in `type` and in extensions, a concept's text and the position.
//! Each leaves the rest of the resource as it was, and removes a list or
//! object an edit leaves empty.

use serde_json::{Map, Value, json};

use crate::fhir::location::coding_code_by_system;
use crate::report::Report;

pub(crate) type Object = Map<String, Value>;

/// `obj[key]` as an array, creating one (or replacing a non-array) first.
pub(crate) fn array_mut<'a>(obj: &'a mut Object, key: &str) -> &'a mut Vec<Value> {
    if !matches!(obj.get(key), Some(Value::Array(_))) {
        obj.insert(key.to_string(), Value::Array(Vec::new()));
    }
    obj.get_mut(key)
        .and_then(Value::as_array_mut)
        .expect("ensured above")
}

/// `obj[key]` as an object, creating one (or replacing a non-object) first.
pub(crate) fn object_mut<'a>(obj: &'a mut Object, key: &str) -> &'a mut Object {
    if !matches!(obj.get(key), Some(Value::Object(_))) {
        obj.insert(key.to_string(), Value::Object(Object::new()));
    }
    obj.get_mut(key)
        .and_then(Value::as_object_mut)
        .expect("ensured above")
}

/// `arr[0]` as an object, inserting an empty one at the front if needed.
pub(crate) fn first_object_mut(arr: &mut Vec<Value>) -> &mut Object {
    if !matches!(arr.first(), Some(Value::Object(_))) {
        arr.insert(0, Value::Object(Object::new()));
    }
    arr[0].as_object_mut().expect("ensured above")
}

pub(crate) fn set_string(obj: &mut Object, key: &str, v: Option<&str>) {
    match v {
        Some(s) => {
            obj.insert(key.to_string(), Value::String(s.to_string()));
        }
        None => {
            obj.remove(key);
        }
    }
}

/// `type` edits the code of the first coding of the first concept and keeps
/// its system. Clearing removes that one coding only: the facility level
/// and ownership concepts share the list.
pub(crate) fn set_first_type_code(obj: &mut Object, code: Option<&str>) {
    let Some(code) = code else {
        if let Some(Value::Array(types)) = obj.get_mut("type") {
            if let Some(Value::Object(first)) = types.first_mut() {
                let now_empty = match first.get_mut("coding").and_then(Value::as_array_mut) {
                    Some(codings) => {
                        if !codings.is_empty() {
                            codings.remove(0);
                        }
                        codings.is_empty()
                    }
                    None => false,
                };
                if now_empty {
                    first.remove("coding");
                }
                if first.is_empty() {
                    types.remove(0);
                }
            }
            if types.is_empty() {
                obj.remove("type");
            }
        }
        return;
    };
    let types = array_mut(obj, "type");
    let first = first_object_mut(types);
    let codings = array_mut(first, "coding");
    let coding = first_object_mut(codings);
    coding.insert("code".into(), Value::String(code.to_string()));
}

pub(crate) fn set_physical_type(obj: &mut Object, code: Option<&str>) {
    let Some(code) = code else {
        obj.remove("physicalType");
        return;
    };
    let concept = object_mut(obj, "physicalType");
    let coding = first_object_mut(array_mut(concept, "coding"));
    coding.insert("code".into(), Value::String(code.to_string()));
}

/// Replace the last path segment of an existing reference, so `Location/x`,
/// `http://host/fhir/Location/x` keep their shape; with no usable existing
/// reference the type is prefixed. Other fields (`display`) are kept.
pub(crate) fn set_reference(obj: &mut Object, key: &str, default_type: &str, id: Option<&str>) {
    let Some(id) = id else {
        obj.remove(key);
        return;
    };
    let prefix = obj
        .get(key)
        .and_then(|r| r.get("reference"))
        .and_then(Value::as_str)
        .and_then(|r| r.rfind('/').map(|i| r[..=i].to_string()));
    let reference = match prefix {
        Some(p) => format!("{p}{id}"),
        None => format!("{default_type}/{id}"),
    };
    object_mut(obj, key).insert("reference".into(), Value::String(reference));
}

/// The system an identifier column's value was read from, so an edit goes
/// back to the same entry: the first of `systems` the resource has, else the
/// first of `systems`.
pub(crate) fn identifier_system_of(obj: &Object, systems: &[String]) -> String {
    let list = obj.get("identifier").and_then(Value::as_array);
    systems
        .iter()
        .find(|system| list.is_some_and(|l| l.iter().any(|i| system_is(i, system))))
        .or(systems.first())
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn system_is(entry: &Value, system: &str) -> bool {
    entry.get("system").and_then(Value::as_str) == Some(system)
}

pub(crate) fn upsert_identifier(obj: &mut Object, system: &str, value: Option<&str>) {
    let list = array_mut(obj, "identifier");
    match value {
        Some(v) => match list.iter_mut().find(|i| system_is(i, system)) {
            Some(Value::Object(i)) => {
                i.insert("value".into(), Value::String(v.to_string()));
            }
            _ => list.push(json!({"system": system, "value": v})),
        },
        None => list.retain(|i| !system_is(i, system)),
    }
    if list.is_empty() {
        obj.remove("identifier");
    }
}

pub(crate) fn url_is(entry: &Value, url: &str) -> bool {
    entry.get("url").and_then(Value::as_str) == Some(url)
}

/// Set the extension's `valueCode`, or with `system` the coding under
/// `system` in its `valueCodeableConcept`. An unchanged value leaves the
/// extension as it is, so a round trip of unedited rows keeps displays, text
/// and other systems' codings. A changed code keeps the concept's other
/// codings and drops what described the old code (its display and the
/// concept text). Clearing removes the coding, and the extension once
/// nothing is left in it.
pub(crate) fn upsert_extension_code(
    obj: &mut Object,
    url: &str,
    system: Option<&str>,
    code: Option<&str>,
) {
    let current = obj
        .get("extension")
        .and_then(Value::as_array)
        .and_then(|exts| exts.iter().find(|e| url_is(e, url)))
        .and_then(|ext| match system {
            Some(s) => coding_code_by_system(ext.get("valueCodeableConcept"), s),
            None => ext
                .get("valueCode")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    if current.as_deref() == code {
        return;
    }
    let exts = array_mut(obj, "extension");
    let pos = exts.iter().position(|e| url_is(e, url));
    match (code, system) {
        (Some(c), None) => match pos.and_then(|i| exts[i].as_object_mut()) {
            Some(e) => {
                e.retain(|k, _| !k.starts_with("value"));
                e.insert("valueCode".into(), Value::String(c.to_string()));
            }
            None => exts.push(json!({"url": url, "valueCode": c})),
        },
        (Some(c), Some(s)) => {
            let coding = json!({"system": s, "code": c});
            let concept = pos
                .and_then(|i| exts[i].as_object_mut())
                .and_then(|e| e.get_mut("valueCodeableConcept"))
                .and_then(Value::as_object_mut);
            match concept {
                Some(cc) => {
                    cc.remove("text");
                    let codings = cc
                        .entry("coding")
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if !codings.is_array() {
                        *codings = Value::Array(Vec::new());
                    }
                    let codings = codings.as_array_mut().expect("an array");
                    match codings.iter().position(|x| system_is(x, s)) {
                        Some(i) => codings[i] = coding,
                        None => codings.push(coding),
                    }
                }
                None => match pos.and_then(|i| exts[i].as_object_mut()) {
                    Some(e) => {
                        e.retain(|k, _| !k.starts_with("value"));
                        e.insert("valueCodeableConcept".into(), json!({"coding": [coding]}));
                    }
                    None => {
                        exts.push(json!({"url": url, "valueCodeableConcept": {"coding": [coding]}}))
                    }
                },
            }
        }
        (None, None) => exts.retain(|e| !url_is(e, url)),
        (None, Some(s)) => {
            if let Some(i) = pos {
                let left = exts[i]
                    .get_mut("valueCodeableConcept")
                    .and_then(|cc| cc.get_mut("coding"))
                    .and_then(Value::as_array_mut)
                    .map(|codings| {
                        codings.retain(|x| !system_is(x, s));
                        codings.len()
                    })
                    .unwrap_or(0);
                if left == 0 {
                    exts.remove(i);
                }
            }
        }
    }
    if exts.is_empty() {
        obj.remove("extension");
    }
}

/// A coding under `system` anywhere in `Location.type`: set its code
/// (dropping the display if the code changes), or add a concept holding it;
/// clearing removes those codings and any concept left empty.
pub(crate) fn upsert_type_coding(obj: &mut Object, system: &str, code: Option<&str>) {
    let types = array_mut(obj, "type");
    match code {
        Some(c) => {
            let found = types
                .iter_mut()
                .filter_map(Value::as_object_mut)
                .flat_map(|concept| {
                    concept
                        .get_mut("coding")
                        .and_then(Value::as_array_mut)
                        .into_iter()
                        .flatten()
                })
                .find(|coding| system_is(coding, system));
            match found {
                Some(Value::Object(coding)) => {
                    // A display belongs to the old code.
                    if coding.get("code").and_then(Value::as_str) != Some(c) {
                        coding.remove("display");
                    }
                    coding.insert("code".into(), Value::String(c.to_string()));
                }
                _ => types.push(json!({"coding": [{"system": system, "code": c}]})),
            }
        }
        None => {
            for concept in types.iter_mut().filter_map(Value::as_object_mut) {
                let now_empty = match concept.get_mut("coding").and_then(Value::as_array_mut) {
                    Some(codings) => {
                        codings.retain(|coding| !system_is(coding, system));
                        codings.is_empty()
                    }
                    None => false,
                };
                if now_empty {
                    concept.remove("coding");
                }
            }
            types.retain(|concept| concept.as_object().is_some_and(|o| !o.is_empty()));
        }
    }
    if types.is_empty() {
        obj.remove("type");
    }
}

pub(crate) fn write_position(obj: &mut Object, lon: f64, lat: f64) {
    let pos = object_mut(obj, "position");
    pos.insert("longitude".into(), json!(lon));
    pos.insert("latitude".into(), json!(lat));
}

/// `text` on the concept in `type` that holds a coding under `system`.
pub(crate) fn set_concept_text(
    obj: &mut Object,
    system: &str,
    text: Option<&str>,
    id: &str,
    column: &str,
    report: &mut Report,
) {
    let resource = obj
        .get("resourceType")
        .and_then(Value::as_str)
        .unwrap_or("resource")
        .to_string();
    let concept = obj
        .get_mut("type")
        .and_then(Value::as_array_mut)
        .and_then(|types| {
            types.iter_mut().filter_map(Value::as_object_mut).find(|c| {
                c.get("coding")
                    .and_then(Value::as_array)
                    .is_some_and(|cs| cs.iter().any(|x| system_is(x, system)))
            })
        });
    match (concept, text) {
        (Some(c), Some(t)) => {
            c.insert("text".into(), Value::String(t.to_string()));
        }
        (Some(c), None) => {
            c.remove("text");
        }
        (None, Some(_)) => report.add(
            "input_column_type",
            id,
            &format!("{column}: the {resource} has no coding under {system} to label; ignored"),
        ),
        (None, None) => {}
    }
}
