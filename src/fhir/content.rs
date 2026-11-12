//! Comparing resources with an earlier run's without keeping them: a
//! resource's content as a 64-bit hash, with some elements left out, and a
//! key as a 128-bit hash. `dhis2 reports --previous` and `import run
//! --previous` both use these.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde_json::Value;

/// How a resource written this run compares with the previous run's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    New,
    Changed,
    Unchanged,
}

/// The elements a content hash leaves out, as a tree of object keys: a key
/// mapped to `None` is left out with everything under it; one mapped to
/// another tree has those keys left out of its value. A path goes through
/// arrays: it applies to every item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ignore {
    keys: BTreeMap<String, Option<Ignore>>,
}

impl Ignore {
    /// The tree of these dotted paths (`meta.lastUpdated`).
    pub fn of(paths: &[&str]) -> Self {
        let mut i = Self::default();
        for p in paths {
            let parts: Vec<&str> = p.split('.').collect();
            i.add(&parts);
        }
        i
    }

    /// Leaves out the element at `path`, a list of object keys. An element
    /// already left out whole stays so; one left out whole now drops what
    /// was left out under it.
    pub fn add<S: AsRef<str>>(&mut self, path: &[S]) {
        let Some((first, rest)) = path.split_first() else {
            return;
        };
        let first = first.as_ref().to_string();
        if rest.is_empty() {
            self.keys.insert(first, None);
            return;
        }
        match self
            .keys
            .entry(first)
            .or_insert_with(|| Some(Ignore::default()))
        {
            None => {}
            Some(sub) => sub.add(rest),
        }
    }
}

/// A resource's content as a 64-bit hash, without the elements in `ignore`.
/// Object keys are taken in sorted order, so key order does not matter.
/// Numbers are compared as JSON writes them: `1` and `1.0` differ.
pub fn content_hash(v: &Value, ignore: &Ignore) -> u64 {
    let mut h = DefaultHasher::new();
    hash_value(v, Some(ignore), &mut h);
    h.finish()
}

/// `ignore` names keys left out of this value's objects, or of its items'
/// when it is an array.
fn hash_value(v: &Value, ignore: Option<&Ignore>, h: &mut DefaultHasher) {
    match v {
        Value::Null => h.write_u8(0),
        Value::Bool(b) => {
            h.write_u8(1);
            b.hash(h);
        }
        Value::Number(n) => {
            h.write_u8(2);
            n.to_string().hash(h);
        }
        Value::String(s) => {
            h.write_u8(3);
            s.hash(h);
        }
        Value::Array(a) => {
            h.write_u8(4);
            h.write_usize(a.len());
            for x in a {
                hash_value(x, ignore, h);
            }
        }
        Value::Object(o) => {
            h.write_u8(5);
            let rule = |k: &str| ignore.and_then(|i| i.keys.get(k));
            let mut keys: Vec<&String> = o
                .keys()
                .filter(|k| !matches!(rule(k), Some(None)))
                .collect();
            keys.sort();
            h.write_usize(keys.len());
            for k in keys {
                k.hash(h);
                let sub = rule(k).and_then(Option::as_ref);
                hash_value(&o[k.as_str()], sub, h);
            }
        }
    }
}

/// A key (an id, or `ResourceType/id`) as 128 bits: two hashes, so that two
/// of millions of keys sharing one is not a practical concern.
pub fn key_hash(key: &str) -> u128 {
    let mut a = DefaultHasher::new();
    key.hash(&mut a);
    let mut b = DefaultHasher::new();
    0x5bd1_e995u32.hash(&mut b);
    key.hash(&mut b);
    (u128::from(a.finish()) << 64) | u128::from(b.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ignored_paths_are_left_out_at_their_depth_only() {
        let ignore = Ignore::of(&["date", "meta.lastUpdated", "group.code.text"]);
        let a = json!({"id": "a", "date": "1", "meta": {"lastUpdated": "x", "tag": "t"},
                       "group": [{"code": {"text": "a", "coding": []}}, {"code": {"text": "b"}}],
                       "note": {"date": "kept"}});
        let mut b = a.clone();
        b["date"] = json!("2");
        b["meta"]["lastUpdated"] = json!("y");
        b["group"][1]["code"]["text"] = json!("c");
        assert_eq!(content_hash(&a, &ignore), content_hash(&b, &ignore));
        // `date` is left out at the top only.
        let mut c = a.clone();
        c["note"]["date"] = json!("changed");
        assert_ne!(content_hash(&a, &ignore), content_hash(&c, &ignore));
        // The rest of an object on the path is compared.
        let mut d = a.clone();
        d["meta"]["tag"] = json!("u");
        assert_ne!(content_hash(&a, &ignore), content_hash(&d, &ignore));
        let mut e = a.clone();
        e["group"][0]["code"]["coding"] = json!([{"code": "x"}]);
        assert_ne!(content_hash(&a, &ignore), content_hash(&e, &ignore));
        // Without the rules, they differ.
        let none = Ignore::default();
        assert_ne!(content_hash(&a, &none), content_hash(&b, &none));
    }

    #[test]
    fn key_order_does_not_matter_and_a_whole_element_wins() {
        let a: Value = serde_json::from_str(r#"{"a": 1, "b": {"c": 2, "d": 3}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"b": {"d": 3, "c": 2}, "a": 1}"#).unwrap();
        let none = Ignore::default();
        assert_eq!(content_hash(&a, &none), content_hash(&b, &none));
        assert_eq!(Ignore::of(&["b.c", "b"]), Ignore::of(&["b"]));
        assert_eq!(Ignore::of(&["b", "b.c"]), Ignore::of(&["b"]));
        let ignore = Ignore::of(&["b"]);
        assert_eq!(
            content_hash(&a, &ignore),
            content_hash(&json!({"a": 1}), &ignore)
        );
        assert_ne!(
            content_hash(&json!({"a": 1}), &none),
            content_hash(&json!({"a": 1.0}), &none)
        );
    }

    #[test]
    fn keys_differ() {
        assert_ne!(key_hash("Patient/a"), key_hash("Location/a"));
        assert_eq!(key_hash("Patient/a"), key_hash("Patient/a"));
    }
}
