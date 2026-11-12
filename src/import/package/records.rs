//! Record files and the logical models they are instances of.
//!
//! The `--in` folder holds one file per record type, `<Type>.ndjson` or
//! `<Type>.ndjson.gz`, one record per line. A record's type is the last path
//! segment of its logical model's URL.
//!
//! A record is checked against its model's snapshot:
//!
//! - every key is an element of the model; `resourceType` and `id` are
//!   always allowed at the top, and `_name` is allowed beside an element
//!   `name` (FHIR JSON's primitive extensions);
//! - every element with `min` of 1 or more is present;
//! - an element with `max` of 1 takes one value, and any other takes a JSON
//!   array, as FHIR JSON requires: a single value for a repeating element is
//!   reported;
//! - a value is not null, and a FHIR primitive has its JSON type: the
//!   string-like types a string, `boolean` a boolean, `integer`,
//!   `positiveInt` and `unsignedInt` a whole number in range, `decimal` a
//!   number;
//! - a BackboneElement, an element that refers to another with
//!   `contentReference`, and an element typed with another model of the
//!   package are checked inside the same way. A FHIR complex type, such as
//!   CodeableConcept, is checked only to be a JSON object.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::{PipesError, Result};
use crate::fhir::ndjson::NdjsonReader;

fn usage(message: String) -> PipesError {
    PipesError::Usage(message)
}

/// The record type of a model URL: its last path segment.
pub fn type_of(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

/// The record files of a folder, by type.
pub fn scan(dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    if !dir.is_dir() {
        return Err(usage(format!(
            "{} is not a folder of record files",
            dir.display()
        )));
    }
    let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
    for path in crate::fs::sorted_entries(dir).map_err(|e| PipesError::io(dir, e))? {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') || !path.is_file() {
            continue;
        }
        let Some(t) = name
            .strip_suffix(".ndjson.gz")
            .or_else(|| name.strip_suffix(".ndjson"))
        else {
            continue;
        };
        if let Some(other) = files.insert(t.to_string(), path.clone()) {
            return Err(usage(format!(
                "{} and {} hold the same record type; keep one",
                other.display(),
                path.display()
            )));
        }
    }
    Ok(files)
}

/// The records of a file, each with its line number. A record without a
/// `resourceType` is given `type_name`; one with another is an error.
pub struct Records {
    path: PathBuf,
    type_name: String,
    lines: NdjsonReader,
}

impl Records {
    pub fn open(path: &Path, type_name: &str) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            type_name: type_name.to_string(),
            lines: NdjsonReader::open(path)?,
        })
    }
}

impl Iterator for Records {
    type Item = Result<(usize, Value)>;

    fn next(&mut self) -> Option<Self::Item> {
        let line = match self.lines.next()? {
            Ok(line) => line,
            Err(e) => return Some(Err(e)),
        };
        let at = format!("{}:{}", self.path.display(), line.number);
        let record = serde_json::from_str::<Value>(&line.text)
            .map_err(|e| usage(format!("{at}: {e}")))
            .and_then(|r| with_type(r, &self.type_name).map_err(|m| usage(format!("{at}: {m}"))));
        Some(record.map(|r| (line.number, r)))
    }
}

/// `record` with `resourceType` set to `type_name` when it has none. A
/// record that is not an object, or names another type, is an error.
pub fn with_type(mut record: Value, type_name: &str) -> std::result::Result<Value, String> {
    let Some(object) = record.as_object_mut() else {
        return Err("a record must be a JSON object".into());
    };
    match object.get("resourceType") {
        None => {
            object.insert("resourceType".into(), Value::String(type_name.into()));
        }
        Some(Value::String(t)) if t == type_name => {}
        Some(other) => {
            return Err(format!(
                "resourceType is {other}, but the file holds {type_name} records"
            ));
        }
    }
    Ok(record)
}

/// How many values an element takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Max {
    One,
    Many,
}

#[derive(Debug, Clone)]
struct Element {
    /// The last segment of the path: `status`, or `value[x]`.
    name: String,
    min: u64,
    max: Max,
    types: Vec<String>,
    /// For `contentReference: "#Model.part"`, `Model.part`.
    content_reference: Option<String>,
}

/// A logical model: its elements by path.
#[derive(Debug, Clone)]
pub struct Model {
    pub url: String,
    pub type_name: String,
    root: String,
    /// Each path's child elements, in snapshot order.
    children: HashMap<String, Vec<Element>>,
}

impl Model {
    pub fn from_json(sd: &Value) -> std::result::Result<Self, String> {
        let url = sd["url"]
            .as_str()
            .ok_or("a StructureDefinition has no url")?
            .to_string();
        let elements = sd["snapshot"]["element"]
            .as_array()
            .filter(|e| !e.is_empty())
            .ok_or_else(|| format!("{url} has no snapshot; the records are checked against it"))?;
        let root = elements[0]["path"]
            .as_str()
            .ok_or_else(|| format!("{url}: the first snapshot element has no path"))?
            .to_string();
        let mut children: HashMap<String, Vec<Element>> = HashMap::new();
        for e in &elements[1..] {
            let path = e["path"]
                .as_str()
                .ok_or_else(|| format!("{url}: a snapshot element has no path"))?;
            let Some((parent, name)) = path.rsplit_once('.') else {
                continue;
            };
            let max = match e["max"].as_str() {
                Some("0") => continue,
                Some("1") => Max::One,
                _ => Max::Many,
            };
            let types = e["type"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t["code"].as_str())
                .map(str::to_string)
                .collect();
            let content_reference = e["contentReference"]
                .as_str()
                .map(|r| r.rsplit_once('#').map_or(r, |(_, p)| p).to_string());
            children
                .entry(parent.to_string())
                .or_default()
                .push(Element {
                    name: name.to_string(),
                    min: e["min"].as_u64().unwrap_or(0),
                    max,
                    types,
                    content_reference,
                });
        }
        Ok(Self {
            type_name: type_of(&url).to_string(),
            url,
            root,
            children,
        })
    }
}

/// The logical models of a package, by record type.
#[derive(Debug, Default)]
pub struct Models {
    by_type: BTreeMap<String, Model>,
    by_url: HashMap<String, String>,
}

impl Models {
    /// Every StructureDefinition JSON under `dir`.
    pub fn load(dir: &Path) -> Result<Self> {
        let mut models = Models::default();
        if !dir.is_dir() {
            return Err(usage(format!(
                "{} is not a folder of logical models",
                dir.display()
            )));
        }
        let mut files = Vec::new();
        json_files(dir, &mut files)?;
        for path in files {
            let text = std::fs::read_to_string(&path).map_err(|e| PipesError::io(&path, e))?;
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| usage(format!("{}: {e}", path.display())))?;
            if v["resourceType"] != "StructureDefinition" {
                continue;
            }
            let model =
                Model::from_json(&v).map_err(|m| usage(format!("{}: {m}", path.display())))?;
            models
                .add(model)
                .map_err(|m| usage(format!("{}: {m}", path.display())))?;
        }
        Ok(models)
    }

    fn add(&mut self, model: Model) -> std::result::Result<(), String> {
        if let Some(other) = self.by_type.get(&model.type_name) {
            return Err(format!(
                "{} and {} are both record type {}",
                other.url, model.url, model.type_name
            ));
        }
        self.by_url
            .insert(model.url.clone(), model.type_name.clone());
        self.by_type.insert(model.type_name.clone(), model);
        Ok(())
    }

    pub fn get(&self, type_name: &str) -> Option<&Model> {
        self.by_type.get(type_name)
    }

    pub fn by_url(&self, url: &str) -> Option<&Model> {
        self.by_url.get(url).and_then(|t| self.by_type.get(t))
    }

    pub fn types(&self) -> impl Iterator<Item = &str> {
        self.by_type.keys().map(String::as_str)
    }

    /// The problems of `record` against the model of `type_name`, each a
    /// short message naming the element by its JSON path.
    pub fn check(&self, type_name: &str, record: &Value) -> Vec<String> {
        let mut out = Vec::new();
        let Some(model) = self.get(type_name) else {
            out.push(format!("no logical model for record type {type_name}"));
            return out;
        };
        let Some(object) = record.as_object() else {
            out.push("a record must be a JSON object".into());
            return out;
        };
        self.check_object(model, &model.root, object, "", true, &mut out);
        out
    }

    fn check_object(
        &self,
        model: &Model,
        path: &str,
        object: &Map<String, Value>,
        at: &str,
        top: bool,
        out: &mut Vec<String>,
    ) {
        let none = Vec::new();
        let children = model.children.get(path).unwrap_or(&none);
        let name = |key: &str| {
            if at.is_empty() {
                key.to_string()
            } else {
                format!("{at}.{key}")
            }
        };
        for (key, value) in object {
            if top && (key == "resourceType" || key == "id") {
                continue;
            }
            if let Some(base) = key.strip_prefix('_')
                && children.iter().any(|e| e.name == base)
            {
                continue;
            }
            match find(children, key) {
                Some((element, type_code)) => {
                    self.check_value(model, path, element, type_code, value, &name(key), out)
                }
                None => out.push(format!("unknown element `{}`", name(key))),
            }
        }
        for element in children.iter().filter(|e| e.min > 0) {
            let present: Vec<&Value> = object
                .iter()
                .filter(|(k, _)| find(std::slice::from_ref(element), k).is_some())
                .map(|(_, v)| v)
                .collect();
            let empty = present
                .iter()
                .all(|v| v.as_array().is_some_and(Vec::is_empty));
            if present.is_empty() || empty {
                out.push(format!(
                    "missing required element `{}`",
                    name(&element.name)
                ));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn check_value(
        &self,
        model: &Model,
        parent: &str,
        element: &Element,
        type_code: Option<&str>,
        value: &Value,
        at: &str,
        out: &mut Vec<String>,
    ) {
        let items: Vec<(String, &Value)> = match (element.max, value) {
            (Max::One, Value::Array(_)) => {
                out.push(format!("`{at}` is an array, but takes one value"));
                return;
            }
            (Max::One, v) => vec![(at.to_string(), v)],
            (Max::Many, Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(i, v)| (format!("{at}[{i}]"), v))
                .collect(),
            (Max::Many, _) => {
                out.push(format!("`{at}` repeats, so its value must be a JSON array"));
                return;
            }
        };
        let path = format!("{parent}.{}", element.name);
        for (at, item) in items {
            if item.is_null() {
                out.push(format!("`{at}` is null"));
                continue;
            }
            if let Some(reference) = &element.content_reference {
                self.check_nested(model, reference, item, &at, out);
                continue;
            }
            let code = match (type_code, element.types.as_slice()) {
                (Some(c), _) => c,
                (None, [one]) => one.as_str(),
                _ => continue,
            };
            if matches!(code, "BackboneElement" | "Element") {
                self.check_nested(model, &path, item, &at, out);
            } else if let Some(other) = self.by_url(code) {
                self.check_nested(other, &other.root, item, &at, out);
            } else if let Some(problem) = primitive(code, item) {
                out.push(format!("`{at}` {problem}"));
            }
        }
    }

    fn check_nested(
        &self,
        model: &Model,
        path: &str,
        item: &Value,
        at: &str,
        out: &mut Vec<String>,
    ) {
        match item.as_object() {
            Some(object) => self.check_object(model, path, object, at, false, out),
            None => out.push(format!("`{at}` must be a JSON object, not {}", kind(item))),
        }
    }
}

/// The element `key` names, and for a choice element (`value[x]`) the type
/// its suffix names (`valueString`: `string`).
fn find<'e>(children: &'e [Element], key: &str) -> Option<(&'e Element, Option<&'e str>)> {
    if let Some(e) = children.iter().find(|e| e.name == key) {
        return Some((e, None));
    }
    children.iter().find_map(|e| {
        let stem = e.name.strip_suffix("[x]")?;
        let suffix = key.strip_prefix(stem)?;
        e.types
            .iter()
            .find(|t| capitalised(t) == suffix)
            .map(|t| (e, Some(t.as_str())))
    })
}

fn capitalised(t: &str) -> String {
    let mut c = t.chars();
    match c.next() {
        Some(first) => first.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// What is wrong with `value` as a FHIR primitive of type `code`, if
/// anything. Complex types must be objects; their content is not checked.
fn primitive(code: &str, value: &Value) -> Option<String> {
    let code = code
        .strip_prefix("http://hl7.org/fhirpath/System.")
        .map_or(code, |s| match s {
            "String" => "string",
            "Boolean" => "boolean",
            "Integer" => "integer",
            "Decimal" => "decimal",
            "Date" => "date",
            "DateTime" => "dateTime",
            "Time" => "time",
            other => other,
        });
    let want = match code {
        "string" | "code" | "id" | "uri" | "url" | "canonical" | "oid" | "uuid" | "markdown"
        | "base64Binary" | "date" | "dateTime" | "instant" | "time" | "xhtml" => {
            if value.is_string() {
                return None;
            }
            "a JSON string"
        }
        "boolean" => {
            if value.is_boolean() {
                return None;
            }
            "a JSON boolean"
        }
        "integer" | "positiveInt" | "unsignedInt" => {
            let whole = value.as_i64().or_else(|| value.as_u64().map(|_| i64::MAX));
            match (code, whole) {
                ("positiveInt", Some(n)) if n < 1 => {
                    return Some(format!("must be 1 or more ({code})"));
                }
                ("unsignedInt", Some(n)) if n < 0 => {
                    return Some(format!("must be 0 or more ({code})"));
                }
                (_, Some(_)) => return None,
                _ => "a whole JSON number",
            }
        }
        "decimal" => {
            if value.is_number() {
                return None;
            }
            "a JSON number"
        }
        c if c.starts_with(|ch: char| ch.is_ascii_uppercase()) => {
            if value.is_object() {
                return None;
            }
            "a JSON object"
        }
        _ => return None,
    };
    Some(format!("must be {want} ({code}), not {}", kind(value)))
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn json_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for path in crate::fs::sorted_entries(dir).map_err(|e| PipesError::io(dir, e))? {
        let hidden = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'));
        if hidden {
            continue;
        }
        if path.is_dir() {
            json_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn element(path: &str, min: u64, max: &str, types: &[&str]) -> Value {
        let types: Vec<Value> = types.iter().map(|t| json!({"code": t})).collect();
        json!({"id": path, "path": path, "min": min, "max": max, "type": types})
    }

    fn models() -> Models {
        let visit = json!({"resourceType": "StructureDefinition",
        "url": "https://example.org/StructureDefinition/Visit", "kind": "logical",
        "snapshot": {"element": [
            {"id": "Visit", "path": "Visit", "min": 0, "max": "*"},
            element("Visit.key", 1, "1", &["string"]),
            element("Visit.count", 0, "1", &["integer"]),
            element("Visit.size", 0, "1", &["positiveInt"]),
            element("Visit.ratio", 0, "1", &["decimal"]),
            element("Visit.done", 0, "1", &["boolean"]),
            element("Visit.when", 0, "1", &["dateTime"]),
            element("Visit.value[x]", 0, "1", &["string", "integer"]),
            element("Visit.code", 0, "1", &["CodeableConcept"]),
            element("Visit.place", 0, "1", &["https://example.org/StructureDefinition/Place"]),
            element("Visit.task", 1, "*", &["BackboneElement"]),
            element("Visit.task.id", 1, "1", &["string"]),
            element("Visit.task.tag", 0, "*", &["code"]),
            element("Visit.task.sub", 0, "*", &[]),
            element("Visit.gone", 0, "0", &["string"]),
        ]}});
        let mut visit = visit;
        visit["snapshot"]["element"][13]["contentReference"] = json!("#Visit.task");
        let place = json!({"resourceType": "StructureDefinition",
        "url": "https://example.org/StructureDefinition/Place", "kind": "logical",
        "snapshot": {"element": [
            {"id": "Place", "path": "Place", "min": 0, "max": "*"},
            element("Place.name", 1, "1", &["string"]),
        ]}});
        let mut m = Models::default();
        m.add(Model::from_json(&visit).unwrap()).unwrap();
        m.add(Model::from_json(&place).unwrap()).unwrap();
        m
    }

    #[test]
    fn a_valid_record_has_no_problems() {
        let r = json!({"resourceType": "Visit", "id": "v1", "key": "k", "count": -2, "size": 3,
            "ratio": 0.5, "done": true, "when": "2026-07-01T09:00:00Z", "valueInteger": 4,
            "code": {"text": "x"}, "place": {"name": "Bo"}, "_key": {"extension": []},
            "task": [{"id": "t1", "tag": ["a"], "sub": [{"id": "t2"}]}]});
        assert_eq!(models().check("Visit", &r), Vec::<String>::new());
    }

    #[test]
    fn unknown_missing_and_wrong_shapes_are_reported_by_path() {
        let r = json!({"key": 1, "kount": 2, "count": 1.5, "size": 0, "ratio": "0.5",
            "done": "yes", "valueBoolean": true, "code": "x", "place": {}, "gone": "x",
            "task": [{"tag": "a", "extra": 1}, null, {"id": "t", "sub": {"id": "s"}}]});
        let problems = models().check("Visit", &r);
        // In the record's key order, each element's own problems at its place.
        let want = [
            "`key` must be a JSON string (string), not a number",
            "unknown element `kount`",
            "`count` must be a whole JSON number (integer), not a number",
            "`size` must be 1 or more (positiveInt)",
            "`ratio` must be a JSON number (decimal), not a string",
            "`done` must be a JSON boolean (boolean), not a string",
            "unknown element `valueBoolean`",
            "`code` must be a JSON object (CodeableConcept), not a string",
            "missing required element `place.name`",
            "unknown element `gone`",
            "`task[0].tag` repeats, so its value must be a JSON array",
            "unknown element `task[0].extra`",
            "missing required element `task[0].id`",
            "`task[1]` is null",
            "`task[2].sub` repeats, so its value must be a JSON array",
        ];
        assert_eq!(problems, want);
        let r = json!({"task": {"id": "t"}});
        assert_eq!(
            models().check("Visit", &r),
            [
                "`task` repeats, so its value must be a JSON array",
                "missing required element `key`"
            ]
        );
        let r = json!({"key": ["k"], "task": []});
        assert_eq!(
            models().check("Visit", &r),
            [
                "`key` is an array, but takes one value",
                "missing required element `task`"
            ]
        );
    }

    #[test]
    fn a_record_names_its_own_type_or_is_given_it() {
        assert_eq!(
            with_type(json!({"a": 1}), "Visit").unwrap(),
            json!({"resourceType": "Visit", "a": 1})
        );
        assert!(with_type(json!({"resourceType": "Visit"}), "Visit").is_ok());
        let m = with_type(json!({"resourceType": "Task"}), "Visit").unwrap_err();
        assert!(m.contains("resourceType is \"Task\""), "{m}");
        assert!(with_type(json!([1]), "Visit").is_err());
        assert_eq!(
            type_of("https://x.org/StructureDefinition/DIGITTask"),
            "DIGITTask"
        );
    }

    #[test]
    fn a_folder_holds_one_file_per_type() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["A.ndjson", "B.ndjson.gz", "notes.txt", ".C.ndjson"] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        let files = scan(dir.path()).unwrap();
        assert_eq!(files.keys().collect::<Vec<_>>(), ["A", "B"]);
        std::fs::write(dir.path().join("A.ndjson.gz"), "").unwrap();
        let err = scan(dir.path()).unwrap_err().to_string();
        assert!(err.contains("hold the same record type"), "{err}");
    }
}
