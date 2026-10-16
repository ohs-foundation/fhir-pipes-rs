//! The few fields of a resource line that indexing and planning a sync
//! need, read without building a `Value` of the whole line: about four
//! times faster on MeasureReports. The result is what reading the line as a
//! `Value` gives, for every line: every other field is parsed and dropped
//! (so a line `Value` rejects, such as one with a number out of range, is
//! rejected too), a repeated field name keeps its last value, and a field of
//! another JSON type than expected reads as absent, as
//! `Value::get(..).and_then(Value::as_str)` would have it.

use std::fmt;

use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, SeqAccess, Visitor};

/// `resourceType`, `id`, `meta.lastUpdated` and `partOf.reference`, when
/// each is a string.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Head {
    pub resource_type: Option<String>,
    pub id: Option<String>,
    pub last_updated: Option<String>,
    pub part_of: Option<String>,
}

impl Head {
    /// The head of one NDJSON line: an error when the line is not JSON (as
    /// `Value` reads it), `None` when it is JSON but not an object.
    pub fn parse(text: &str) -> serde_json::Result<Option<Head>> {
        Ok(serde_json::from_str::<Top>(text)?.0)
    }
}

/// The field names read; any other is `Other`.
#[derive(Deserialize)]
#[serde(field_identifier)]
enum Key {
    #[serde(rename = "resourceType")]
    ResourceType,
    #[serde(rename = "id")]
    Id,
    #[serde(rename = "meta")]
    Meta,
    #[serde(rename = "partOf")]
    PartOf,
    #[serde(rename = "lastUpdated")]
    LastUpdated,
    #[serde(rename = "reference")]
    Reference,
    #[serde(other)]
    Other,
}

/// Any JSON value, parsed as `Value` would parse it and dropped.
struct Skip;

/// A string, or `None` for any other JSON value.
struct Str(Option<String>);

/// The line: `None` unless it is an object.
struct Top(Option<Head>);

/// An object's string field `K` (`meta.lastUpdated`, `partOf.reference`);
/// `None` for anything else.
struct Field<const K: u8>(Option<String>);

const LAST_UPDATED: u8 = 0;
const REFERENCE: u8 = 1;

/// The visits every reader here shares: scalars are "not what was asked
/// for", and arrays are parsed and dropped. Objects are each reader's own.
macro_rules! other_values {
    ($absent:expr) => {
        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok($absent)
        }
        fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
            Ok($absent)
        }
        fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
            Ok($absent)
        }
        fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
            Ok($absent)
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok($absent)
        }
        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok($absent)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<Skip>()?.is_some() {}
            Ok($absent)
        }
    };
}

/// Parse and drop every entry of an object.
fn skip_map<'de, A: MapAccess<'de>>(mut map: A) -> Result<(), A::Error> {
    while map.next_entry::<Skip, Skip>()?.is_some() {}
    Ok(())
}

impl<'de> Deserialize<'de> for Skip {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Skip;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_str<E>(self, _: &str) -> Result<Skip, E> {
                Ok(Skip)
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Skip, A::Error> {
                skip_map(map).map(|()| Skip)
            }
            other_values!(Skip);
        }
        // `deserialize_any`, not `deserialize_ignored_any`: serde_json skips
        // an ignored value without checking its numbers' range or its
        // strings' escapes, which `Value` checks.
        d.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for Str {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Str;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_str<E>(self, v: &str) -> Result<Str, E> {
                Ok(Str(Some(v.to_string())))
            }
            fn visit_string<E>(self, v: String) -> Result<Str, E> {
                Ok(Str(Some(v)))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Str, A::Error> {
                skip_map(map).map(|()| Str(None))
            }
            other_values!(Str(None));
        }
        d.deserialize_any(V)
    }
}

impl<'de, const K: u8> Deserialize<'de> for Field<K> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<const K: u8>;
        impl<'de, const K: u8> Visitor<'de> for V<K> {
            type Value = Field<K>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_str<E>(self, _: &str) -> Result<Field<K>, E> {
                Ok(Field(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Field<K>, A::Error> {
                let mut found = None;
                while let Some(key) = map.next_key::<Key>()? {
                    match (K, key) {
                        (LAST_UPDATED, Key::LastUpdated) | (REFERENCE, Key::Reference) => {
                            found = map.next_value::<Str>()?.0;
                        }
                        _ => {
                            map.next_value::<Skip>()?;
                        }
                    }
                }
                Ok(Field(found))
            }
            other_values!(Field(None));
        }
        d.deserialize_any(V::<K>)
    }
}

impl<'de> Deserialize<'de> for Top {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Top;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_str<E>(self, _: &str) -> Result<Top, E> {
                Ok(Top(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Top, A::Error> {
                // A repeated name overwrites, as in a `Value` object.
                let mut head = Head::default();
                while let Some(key) = map.next_key::<Key>()? {
                    match key {
                        Key::ResourceType => head.resource_type = map.next_value::<Str>()?.0,
                        Key::Id => head.id = map.next_value::<Str>()?.0,
                        Key::Meta => {
                            head.last_updated = map.next_value::<Field<LAST_UPDATED>>()?.0;
                        }
                        Key::PartOf => head.part_of = map.next_value::<Field<REFERENCE>>()?.0,
                        _ => {
                            map.next_value::<Skip>()?;
                        }
                    }
                }
                Ok(Top(Some(head)))
            }
            other_values!(Top(None));
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// What the `Value` reads gave, for comparison.
    fn by_value(text: &str) -> serde_json::Result<Option<Head>> {
        let v: Value = serde_json::from_str(text)?;
        if !v.is_object() {
            return Ok(None);
        }
        let s = |ptr: &str| v.pointer(ptr).and_then(Value::as_str).map(str::to_string);
        Ok(Some(Head {
            resource_type: s("/resourceType"),
            id: s("/id"),
            last_updated: s("/meta/lastUpdated"),
            part_of: s("/partOf/reference"),
        }))
    }

    #[test]
    fn the_head_is_what_a_full_parse_finds() {
        for text in [
            r#"{"resourceType":"Location","id":"a","meta":{"versionId":"2","lastUpdated":"2026-01-01T00:00:00Z"},"partOf":{"reference":"Location/p"}}"#,
            r#"{"id":"a","extension":[{"url":"x","valueAttachment":{"data":"..."}}],"meta":{"tag":[{"code":"c"}]}}"#,
            r#"{"id":"a\n","meta":{"lastUpdated":"2026"},"resourceType":"Patient"}"#,
            r#"{"id":5,"resourceType":["x"],"meta":"m","partOf":{"reference":{"x":1}}}"#,
            r#"{"id":null,"meta":{"lastUpdated":7},"partOf":[1,{"a":[]}]}"#,
            r#"{"meta":null,"partOf":null,"n":1.5e300,"b":true}"#,
            r#"{}"#,
            r#"["a"]"#,
            r#""a""#,
            r#"7"#,
            r#"null"#,
        ] {
            let got = Head::parse(text).unwrap();
            assert_eq!(got, by_value(text).unwrap(), "{text}");
        }
    }

    #[test]
    fn a_repeated_field_keeps_its_last_value_as_in_a_value() {
        for text in [
            r#"{"resourceType":"Patient","resourceType":"Patient","id":"a"}"#,
            r#"{"resourceType":"Patient","id":"a","id":"b"}"#,
            r#"{"id":"a","id":5}"#,
            r#"{"id":"a","meta":{"lastUpdated":"b"},"meta":{"lastUpdated":"c"}}"#,
            r#"{"id":"a","meta":{"lastUpdated":"b"},"meta":{"versionId":"2"}}"#,
            r#"{"id":"a","meta":{"lastUpdated":"b","lastUpdated":"c"}}"#,
            r#"{"partOf":{"reference":"Location/x"},"partOf":"y"}"#,
            r#"{"partOf":{"reference":"a","reference":"b"}}"#,
        ] {
            assert_eq!(
                Head::parse(text).unwrap(),
                by_value(text).unwrap(),
                "{text}"
            );
        }
        let head = Head::parse(r#"{"resourceType":"Patient","id":"a","id":"b"}"#).unwrap();
        assert_eq!(head.unwrap().id.as_deref(), Some("b"));
    }

    #[test]
    fn a_line_a_value_rejects_is_rejected() {
        for text in [
            r#"{"id":"a","x":"\ud800"}"#,
            r#"{"id":"a","x":{"y":["\udc00x"]}}"#,
            r#"{"id":"a","x":1e400}"#,
            r#"{"id":"a","meta":{"tag":[-1e400]}}"#,
            r#"{"id":"a","\ud800":1}"#,
            r#"{"id":"\ud800"}"#,
        ] {
            assert!(by_value(text).is_err(), "{text}");
            assert!(Head::parse(text).is_err(), "{text}");
        }
        // A surrogate pair is a character.
        let pair = r#"{"id":"a","x":"\ud83d\ude00"}"#;
        assert_eq!(Head::parse(pair).unwrap(), by_value(pair).unwrap());
    }

    #[test]
    fn a_line_that_is_not_json_is_an_error() {
        for text in [
            "not json",
            r#"{"id":"a""#,
            r#"{"id":"a"} x"#,
            r#"{"id":"a","x":[1,}"#,
        ] {
            assert!(Head::parse(text).is_err(), "{text}");
            assert!(by_value(text).is_err(), "{text}");
        }
    }

    /// Every line of a real file, when one is given, both ways.
    #[test]
    #[ignore = "needs FHIR_PIPES_BENCH_NDJSON"]
    fn the_head_of_every_line_of_a_real_file_is_what_a_full_parse_finds() {
        let Ok(path) = std::env::var("FHIR_PIPES_BENCH_NDJSON") else {
            return;
        };
        let text = std::fs::read_to_string(path).unwrap();
        let started = std::time::Instant::now();
        let full: Vec<_> = text.lines().map(|l| by_value(l).unwrap()).collect();
        let full_time = started.elapsed();
        let started = std::time::Instant::now();
        let heads: Vec<_> = text.lines().map(|l| Head::parse(l).unwrap()).collect();
        let head_time = started.elapsed();
        eprintln!(
            "{} lines, {} MB: Value {full_time:?}, Head {head_time:?}",
            heads.len(),
            text.len() / 1_000_000
        );
        assert_eq!(full, heads);
    }
}
