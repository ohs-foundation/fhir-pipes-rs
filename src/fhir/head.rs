//! The few fields of a resource line that indexing, merging and planning a
//! sync need, read without building a `Value` of the whole line: about four
//! times faster on MeasureReports. Every other field is skipped, though
//! still checked as JSON. A field of another JSON type than expected reads
//! as absent, as `Value::get(..).and_then(Value::as_str)` would have it.

use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::value::MapAccessDeserializer;
use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

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
    /// The head of one NDJSON line: an error when the line is not JSON,
    /// `None` when it is JSON but not an object. A repeated field name is an
    /// error.
    pub fn parse(text: &str) -> serde_json::Result<Option<Head>> {
        let raw: Obj<Raw> = serde_json::from_str(text)?;
        Ok(raw.0.map(|r| Head {
            resource_type: r.resource_type.0,
            id: r.id.0,
            last_updated: r.meta.0.and_then(|m| m.last_updated.0),
            part_of: r.part_of.0.and_then(|p| p.reference.0),
        }))
    }
}

#[derive(Deserialize)]
struct Raw {
    #[serde(rename = "resourceType", default)]
    resource_type: Str,
    #[serde(default)]
    id: Str,
    #[serde(default)]
    meta: Obj<Meta>,
    #[serde(rename = "partOf", default)]
    part_of: Obj<Reference>,
}

#[derive(Deserialize)]
struct Meta {
    #[serde(rename = "lastUpdated", default)]
    last_updated: Str,
}

#[derive(Deserialize)]
struct Reference {
    #[serde(default)]
    reference: Str,
}

/// A string, or `None` for any other JSON value.
#[derive(Default)]
struct Str(Option<String>);

/// An object read as `T`, or `None` for any other JSON value.
struct Obj<T>(Option<T>);

impl<T> Default for Obj<T> {
    fn default() -> Self {
        Obj(None)
    }
}

/// The visits that read a JSON value as absent: scalars, and arrays and
/// objects skipped whole.
macro_rules! absent_for_other_values {
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
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok($absent)
        }
    };
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
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Str, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Str(None))
            }
            absent_for_other_values!(Str(None));
        }
        d.deserialize_any(V)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Obj<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Obj<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Obj<T>, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(|t| Obj(Some(t)))
            }
            fn visit_str<E>(self, _: &str) -> Result<Obj<T>, E> {
                Ok(Obj(None))
            }
            absent_for_other_values!(Obj(None));
        }
        d.deserialize_any(V(PhantomData))
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
