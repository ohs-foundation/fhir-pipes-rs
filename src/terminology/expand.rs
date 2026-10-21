//! The members of a value set, from its `expansion` or its `compose`.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Result, anyhow};
use serde_json::Value;

use super::{Content, Members, array};
use crate::error::usage;

/// One member of a value set: a row of its table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub system: String,
    /// The code system version used, where one is recorded.
    pub version: Option<String>,
    pub code: String,
    pub display: Option<String>,
    /// `true` for a code inactive in that version; `None` where nothing says.
    pub inactive: Option<bool>,
}

impl Member {
    fn key(&self) -> (String, Option<String>, String) {
        (self.system.clone(), self.version.clone(), self.code.clone())
    }
}

/// The members of the value set `canonical` names. `stack` holds the value
/// sets being expanded, to catch one that includes itself.
pub(super) fn members(
    content: &Content,
    canonical: &str,
    stack: &mut Vec<String>,
) -> Result<Vec<Member>> {
    let vs = content.value_set(canonical)?;
    let label = match vs["version"].as_str() {
        Some(v) => format!("{}|{v}", vs["url"].as_str().unwrap_or_default()),
        None => vs["url"].as_str().unwrap_or_default().to_string(),
    };
    if stack.contains(&label) {
        return Err(usage(anyhow!("ValueSet {label} includes itself")));
    }
    stack.push(label.clone());
    let members = if vs.get("expansion").is_some() {
        from_expansion(&vs["expansion"], &label)
    } else if vs.get("compose").is_some() {
        from_compose(content, &vs["compose"], &label, stack)
    } else {
        Err(usage(anyhow!(
            "ValueSet {label} has neither an expansion nor a compose"
        )))
    };
    stack.pop();
    Ok(members?.into_values().collect())
}

/// One member per distinct (system, version, code) in `expansion.contains`
/// at any depth; an `abstract` entry is not a member. An expansion with an
/// offset, or with a `total` above the entries it holds, is a page of a
/// larger one, and fails.
fn from_expansion(expansion: &Value, label: &str) -> Result<Members> {
    let incomplete = |why: &str| {
        usage(anyhow!(
            "ValueSet {label}: the expansion is incomplete ({why})"
        ))
    };
    if expansion["offset"].as_i64().is_some_and(|o| o > 0) {
        return Err(incomplete("it has an offset"));
    }
    let mut out = Members::new();
    let mut entries = 0;
    // Walked in document order, so a code listed twice keeps its first display.
    let mut stack: Vec<&Value> = array(&expansion["contains"]).rev().collect();
    while let Some(entry) = stack.pop() {
        entries += 1;
        stack.extend(array(&entry["contains"]).rev());
        if entry["abstract"] == Value::Bool(true) {
            continue;
        }
        let (Some(system), Some(code)) = (entry["system"].as_str(), entry["code"].as_str()) else {
            continue;
        };
        let member = Member {
            system: system.to_string(),
            version: entry["version"].as_str().map(str::to_string),
            code: code.to_string(),
            display: entry["display"].as_str().map(str::to_string),
            inactive: entry["inactive"].as_bool(),
        };
        out.entry(member.key()).or_insert(member);
    }
    if let Some(total) = expansion["total"].as_i64()
        && total > entries
    {
        return Err(incomplete(&format!(
            "total is {total}, but it holds {entries} entries"
        )));
    }
    Ok(out)
}

/// The members the `include`s give, less those the `exclude`s give; without
/// inactive codes when `compose.inactive` is false.
fn from_compose(
    content: &Content,
    compose: &Value,
    label: &str,
    stack: &mut Vec<String>,
) -> Result<Members> {
    let mut out = Members::new();
    for include in array(&compose["include"]) {
        for member in part(content, include, label, stack)? {
            out.entry(member.key()).or_insert(member);
        }
    }
    let mut excluded = HashSet::new();
    for exclude in array(&compose["exclude"]) {
        for member in part(content, exclude, label, stack)? {
            excluded.insert((member.system, member.code));
        }
    }
    out.retain(|(system, _, code), m| {
        !excluded.contains(&(system.clone(), code.clone()))
            && !(compose["inactive"] == Value::Bool(false) && m.inactive == Some(true))
    });
    Ok(out)
}

/// The codes one `include` or `exclude` names: from its system (listed
/// codes, filters, or the whole code system), and in every value set it
/// names.
fn part(
    content: &Content,
    part: &Value,
    label: &str,
    stack: &mut Vec<String>,
) -> Result<Vec<Member>> {
    let from_system = match part["system"].as_str() {
        Some(system) => Some(from_system(content, part, system, label)?),
        None => None,
    };
    let mut sets = Vec::new();
    for canonical in array(&part["valueSet"]).filter_map(Value::as_str) {
        sets.push(members(content, canonical, stack)?);
    }
    let codes: Vec<HashSet<(&str, &str)>> = sets
        .iter()
        .map(|set| {
            set.iter()
                .map(|m| (m.system.as_str(), m.code.as_str()))
                .collect()
        })
        .collect();
    let in_every_set = |m: &Member| {
        codes
            .iter()
            .all(|c| c.contains(&(m.system.as_str(), m.code.as_str())))
    };
    let candidates = match from_system {
        Some(members) => members,
        // Only value sets: the members of the first that are in the others.
        None if !sets.is_empty() => sets[0].clone(),
        None => {
            return Err(usage(anyhow!(
                "ValueSet {label}: an include or exclude has neither a system nor a valueSet"
            )));
        }
    };
    Ok(candidates.into_iter().filter(in_every_set).collect())
}

/// The codes of `system` that one `include` or `exclude` names.
fn from_system(content: &Content, part: &Value, system: &str, label: &str) -> Result<Vec<Member>> {
    let version = part["version"].as_str();
    let cs = content.code_system(system, version)?;
    let version = version
        .map(str::to_string)
        .or_else(|| cs.and_then(|cs| cs["version"].as_str().map(str::to_string)));
    let concepts = cs.map(Concepts::of).unwrap_or_default();
    let member = |code: &str, display: Option<&str>| Member {
        system: system.to_string(),
        version: version.clone(),
        code: code.to_string(),
        display: display
            .or_else(|| concepts.get(code).and_then(|c| c.display.as_deref()))
            .map(str::to_string),
        inactive: concepts.get(code).and_then(|c| c.inactive),
    };

    let listed: Vec<&Value> = array(&part["concept"]).collect();
    if !listed.is_empty() {
        return Ok(listed
            .into_iter()
            .filter_map(|c| Some(member(c["code"].as_str()?, c["display"].as_str())))
            .collect());
    }

    // Filters and whole code systems need the code system in full.
    let what = if part.get("filter").is_some() {
        "filters"
    } else {
        "includes all of"
    };
    let Some(cs) = cs else {
        return Err(usage(anyhow!(
            "ValueSet {label} {what} {system}, which is not in the packages. Add the \
             package that has it, or expand the value set with a terminology server"
        )));
    };
    let cs_content = cs["content"].as_str().unwrap_or("complete");
    if cs_content != "complete" {
        return Err(usage(anyhow!(
            "ValueSet {label} {what} {system}, but the packages hold only part of it \
             (content = {cs_content})"
        )));
    }

    let mut codes: BTreeSet<&str> = concepts
        .by_code
        .iter()
        .filter(|(_, c)| !c.not_selectable)
        .map(|(code, _)| code.as_str())
        .collect();
    for filter in array(&part["filter"]) {
        let (property, op, value) = (
            filter["property"].as_str().unwrap_or_default(),
            filter["op"].as_str().unwrap_or_default(),
            filter["value"].as_str().unwrap_or_default(),
        );
        let kept: HashSet<&str> = match (property, op) {
            ("concept", "is-a") => concepts.descendants(value, true),
            ("concept", "descendent-of") => concepts.descendants(value, false),
            ("concept", "is-not-a") => {
                let below = concepts.descendants(value, true);
                codes
                    .iter()
                    .copied()
                    .filter(|c| !below.contains(c))
                    .collect()
            }
            _ => {
                return Err(usage(anyhow!(
                    "ValueSet {label} filters {system} by `{property} {op} {value}`; only \
                     concept is-a, descendent-of and is-not-a can be expanded without a \
                     terminology server"
                )));
            }
        };
        codes.retain(|c| kept.contains(c));
    }
    Ok(codes.into_iter().map(|code| member(code, None)).collect())
}

/// A code system's concepts, with the hierarchy their nesting and their
/// `parent` properties give.
#[derive(Debug, Default)]
struct Concepts {
    by_code: HashMap<String, Concept>,
    children: HashMap<String, Vec<String>>,
}

#[derive(Debug, Default)]
struct Concept {
    display: Option<String>,
    inactive: Option<bool>,
    not_selectable: bool,
}

impl Concepts {
    fn of(cs: &Value) -> Self {
        let mut out = Concepts::default();
        out.walk(&cs["concept"], None);
        out
    }

    fn walk(&mut self, concepts: &Value, parent: Option<&str>) {
        for c in array(concepts) {
            let Some(code) = c["code"].as_str() else {
                continue;
            };
            let property =
                |name: &str| array(&c["property"]).find(|p| p["code"].as_str() == Some(name));
            let inactive = property("inactive")
                .and_then(|p| p["valueBoolean"].as_bool())
                .or_else(|| {
                    property("status")
                        .and_then(|p| p["valueCode"].as_str())
                        .map(|s| matches!(s, "retired" | "inactive"))
                });
            let not_selectable = ["notSelectable", "abstract"]
                .iter()
                .any(|n| property(n).and_then(|p| p["valueBoolean"].as_bool()) == Some(true));
            self.by_code.insert(
                code.to_string(),
                Concept {
                    display: c["display"].as_str().map(str::to_string),
                    inactive,
                    not_selectable,
                },
            );
            let parents = parent.into_iter().map(str::to_string).chain(
                array(&c["property"])
                    .filter(|p| matches!(p["code"].as_str(), Some("parent" | "subsumedBy")))
                    .filter_map(|p| p["valueCode"].as_str().map(str::to_string)),
            );
            for p in parents.collect::<Vec<_>>() {
                self.children.entry(p).or_default().push(code.to_string());
            }
            self.walk(&c["concept"], Some(code));
        }
    }

    fn get(&self, code: &str) -> Option<&Concept> {
        self.by_code.get(code)
    }

    /// The codes below `code`, and `code` itself with `and_self`.
    fn descendants(&self, code: &str, and_self: bool) -> HashSet<&str> {
        let mut out = HashSet::new();
        let mut stack: Vec<&str> = self
            .children
            .get(code)
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        while let Some(c) = stack.pop() {
            if out.insert(c) {
                stack.extend(
                    self.children
                        .get(c)
                        .into_iter()
                        .flatten()
                        .map(String::as_str),
                );
            }
        }
        if and_self && let Some((code, _)) = self.by_code.get_key_value(code) {
            out.insert(code.as_str());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    const CS: &str = "http://example.org/CodeSystem/c";

    fn content(resources: Vec<Value>) -> Content {
        let mut c = Content::default();
        for r in resources {
            c.add(r);
        }
        c
    }

    fn vs(id: &str, body: Value) -> Value {
        let mut v = json!({"resourceType": "ValueSet",
                           "url": format!("http://example.org/ValueSet/{id}")});
        v.as_object_mut()
            .unwrap()
            .extend(body.as_object().unwrap().clone());
        v
    }

    fn expand(c: &Content, id: &str) -> Result<Vec<String>> {
        let mut stack = Vec::new();
        let members = members(c, &format!("http://example.org/ValueSet/{id}"), &mut stack)?;
        assert!(stack.is_empty(), "the stack is left as it was");
        Ok(members.into_iter().map(|m| m.code).collect())
    }

    fn concepts(concept: Value) -> Concepts {
        Concepts::of(&json!({"concept": concept}))
    }

    fn sorted(set: HashSet<&str>) -> Vec<&str> {
        let mut v: Vec<&str> = set.into_iter().collect();
        v.sort();
        v
    }

    #[test]
    fn concept_properties_give_inactive_and_not_selectable() {
        let c = concepts(json!([
            {"code": "a", "property": [{"code": "inactive", "valueBoolean": true}]},
            {"code": "b", "property": [{"code": "inactive", "valueBoolean": false},
                                       {"code": "status", "valueCode": "retired"}]},
            {"code": "c", "property": [{"code": "status", "valueCode": "active"}]},
            {"code": "d", "property": [{"code": "abstract", "valueBoolean": true}]},
            {"code": "e"},
            {"display": "no code"}
        ]));
        assert_eq!(c.get("a").unwrap().inactive, Some(true));
        // `inactive` wins over `status`.
        assert_eq!(c.get("b").unwrap().inactive, Some(false));
        assert_eq!(c.get("c").unwrap().inactive, Some(false));
        assert_eq!(c.get("e").unwrap().inactive, None);
        assert!(c.get("d").unwrap().not_selectable);
        assert!(!c.get("e").unwrap().not_selectable);
        assert_eq!(c.by_code.len(), 5);
    }

    #[test]
    fn descendants_follow_nesting_parent_and_subsumed_by_and_stop_at_a_cycle() {
        let c = concepts(json!([
            {"code": "top", "concept": [{"code": "mid", "concept": [{"code": "low"}]}]},
            {"code": "side", "property": [{"code": "subsumedBy", "valueCode": "top"}]},
            // A cycle: p is under q, and q under p.
            {"code": "p", "property": [{"code": "parent", "valueCode": "q"}]},
            {"code": "q", "property": [{"code": "parent", "valueCode": "p"}]}
        ]));
        assert_eq!(
            sorted(c.descendants("top", true)),
            ["low", "mid", "side", "top"]
        );
        assert_eq!(sorted(c.descendants("mid", false)), ["low"]);
        assert_eq!(sorted(c.descendants("p", false)), ["p", "q"]);
        // A code the system does not have has no descendants, nor itself.
        assert!(c.descendants("nothing", true).is_empty());
    }

    #[test]
    fn entries_without_a_system_or_code_are_not_members_but_count_toward_the_total() {
        let c = content(vec![vs(
            "e",
            json!({"expansion": {"total": 3, "contains": [
                {"system": CS, "code": "a"},
                {"code": "no-system"},
                {"system": CS, "display": "no code"}
            ]}}),
        )]);
        assert_eq!(expand(&c, "e").unwrap(), ["a"]);
    }

    #[test]
    fn the_same_code_in_two_versions_is_two_members() {
        let c = content(vec![vs(
            "e",
            json!({"expansion": {"contains": [
                {"system": CS, "version": "1", "code": "a"},
                {"system": CS, "version": "2", "code": "a"},
                {"system": CS, "version": "1", "code": "a"}
            ]}}),
        )]);
        assert_eq!(expand(&c, "e").unwrap(), ["a", "a"]);
    }

    #[test]
    fn an_exclude_removes_a_code_in_every_version_and_can_name_a_value_set() {
        let url = |id: &str| format!("http://example.org/ValueSet/{id}");
        let c = content(vec![
            vs(
                "drop",
                json!({"compose": {"include": [{"system": CS, "concept": [{"code": "b"}]}]}}),
            ),
            vs(
                "all",
                json!({"compose": {
                    "include": [
                        {"system": CS, "version": "1", "concept": [{"code": "a"}, {"code": "b"}]},
                        {"system": CS, "version": "2", "concept": [{"code": "b"}, {"code": "c"}]}
                    ],
                    "exclude": [{"valueSet": [url("drop")]}]
                }}),
            ),
        ]);
        assert_eq!(expand(&c, "all").unwrap(), ["a", "c"]);
    }

    #[test]
    fn a_value_set_included_twice_is_not_a_loop() {
        let url = |id: &str| format!("http://example.org/ValueSet/{id}");
        let c = content(vec![
            vs(
                "base",
                json!({"compose": {"include": [{"system": CS, "concept": [{"code": "a"}]}]}}),
            ),
            vs(
                "one",
                json!({"compose": {"include": [{"valueSet": [url("base")]}]}}),
            ),
            vs(
                "both",
                json!({"compose": {"include": [
                    {"valueSet": [url("base")]}, {"valueSet": [url("one")]}
                ]}}),
            ),
        ]);
        assert_eq!(expand(&c, "both").unwrap(), ["a"]);
    }

    #[test]
    fn a_value_set_with_nothing_to_expand_fails() {
        let c = content(vec![
            vs("bare", json!({})),
            vs(
                "empty-part",
                json!({"compose": {"include": [{"concept": []}]}}),
            ),
        ]);
        let bare = format!("{:#}", expand(&c, "bare").unwrap_err());
        assert!(
            bare.contains("has neither an expansion nor a compose"),
            "{bare}"
        );
        let part = format!("{:#}", expand(&c, "empty-part").unwrap_err());
        assert!(
            part.contains("an include or exclude has neither a system nor a valueSet"),
            "{part}"
        );
    }
}
