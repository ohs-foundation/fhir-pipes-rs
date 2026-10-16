//! The few fields of a resource line that indexing and planning a sync or
//! a load need, read without building a `Value` of the whole line: about four
//! times faster on MeasureReports. The result is what reading the line as a
//! `Value` gives, for every line: every other field is parsed and dropped
//! (so a line `Value` rejects, such as one with a number out of range, is
//! rejected too), a repeated field name keeps its last value, and a field of
//! another JSON type than expected reads as absent, as
//! `Value::get(..).and_then(Value::as_str)` would have it. A line the fast
//! reader does not take, including any object with serde_json's raw-value
//! key (which `Value` reads specially with the `raw_value` feature), is read
//! as a `Value` instead: its answer, or its error.

use std::fmt;

use serde::Deserialize;
use serde::de::{Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// `resourceType`, `id`, `meta.versionId`, `meta.lastUpdated` and
/// `partOf.reference`, when each is a string.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Head {
    pub resource_type: Option<String>,
    pub id: Option<String>,
    pub version_id: Option<String>,
    pub last_updated: Option<String>,
    pub part_of: Option<String>,
}

impl Head {
    /// The head of one NDJSON line: an error when the line is not JSON (as
    /// `Value` reads it), `None` when it is JSON but not an object.
    pub fn parse(text: &str) -> serde_json::Result<Option<Head>> {
        fast(text).or_else(|_| by_value(text))
    }
}

/// The hand-written reader. An error is not final: `parse` then reads the
/// line as a `Value`.
fn fast(text: &str) -> serde_json::Result<Option<Head>> {
    Ok(serde_json::from_str::<Top>(text)?.0)
}

/// The line read as a `Value`, the reference the fast reader matches.
fn by_value(text: &str) -> serde_json::Result<Option<Head>> {
    let v: Value = serde_json::from_str(text)?;
    if !v.is_object() {
        return Ok(None);
    }
    let s = |ptr: &str| v.pointer(ptr).and_then(Value::as_str).map(str::to_string);
    Ok(Some(Head {
        resource_type: s("/resourceType"),
        id: s("/id"),
        version_id: s("/meta/versionId"),
        last_updated: s("/meta/lastUpdated"),
        part_of: s("/partOf/reference"),
    }))
}

/// serde_json's key for a raw value. With its `raw_value` feature on, a
/// `Value` read of an object whose first key is this parses the string
/// that follows as JSON; the fast reader leaves such a line to `Value`.
const RAW_VALUE_KEY: &str = "$serde_json::private::RawValue";

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
    #[serde(rename = "versionId")]
    VersionId,
    #[serde(rename = "lastUpdated")]
    LastUpdated,
    #[serde(rename = "reference")]
    Reference,
    #[serde(rename = "$serde_json::private::RawValue")]
    RawValue,
    #[serde(other)]
    Other,
}

/// Any JSON value, parsed as `Value` would parse it and dropped.
struct Skip;

/// A string, or `None` for any other JSON value.
struct Str(Option<String>);

/// The line: `None` unless it is an object.
struct Top(Option<Head>);

/// `partOf.reference`; `None` for anything else.
struct Reference(Option<String>);

/// `meta.versionId` and `meta.lastUpdated`; both `None` for anything but
/// an object.
#[derive(Default)]
struct Meta {
    version_id: Option<String>,
    last_updated: Option<String>,
}

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
    while let Some(key) = map.next_key::<Key>()? {
        raw_value_key::<A>(&key)?;
        map.next_value::<Skip>()?;
    }
    Ok(())
}

/// An error for serde_json's raw-value key, wherever it is: the line is
/// read as a `Value` instead.
fn raw_value_key<'de, A: MapAccess<'de>>(key: &Key) -> Result<(), A::Error> {
    match key {
        Key::RawValue => Err(A::Error::custom(format!(
            "{RAW_VALUE_KEY}: read as a Value"
        ))),
        _ => Ok(()),
    }
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

impl<'de> Deserialize<'de> for Reference {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Reference;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_str<E>(self, _: &str) -> Result<Reference, E> {
                Ok(Reference(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Reference, A::Error> {
                let mut found = None;
                while let Some(key) = map.next_key::<Key>()? {
                    raw_value_key::<A>(&key)?;
                    match key {
                        Key::Reference => found = map.next_value::<Str>()?.0,
                        _ => {
                            map.next_value::<Skip>()?;
                        }
                    }
                }
                Ok(Reference(found))
            }
            other_values!(Reference(None));
        }
        d.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for Meta {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Meta;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_str<E>(self, _: &str) -> Result<Meta, E> {
                Ok(Meta::default())
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Meta, A::Error> {
                let mut meta = Meta::default();
                while let Some(key) = map.next_key::<Key>()? {
                    raw_value_key::<A>(&key)?;
                    match key {
                        Key::VersionId => meta.version_id = map.next_value::<Str>()?.0,
                        Key::LastUpdated => meta.last_updated = map.next_value::<Str>()?.0,
                        _ => {
                            map.next_value::<Skip>()?;
                        }
                    }
                }
                Ok(meta)
            }
            other_values!(Meta::default());
        }
        d.deserialize_any(V)
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
                    raw_value_key::<A>(&key)?;
                    match key {
                        Key::ResourceType => head.resource_type = map.next_value::<Str>()?.0,
                        Key::Id => head.id = map.next_value::<Str>()?.0,
                        Key::Meta => {
                            let meta = map.next_value::<Meta>()?;
                            head.version_id = meta.version_id;
                            head.last_updated = meta.last_updated;
                        }
                        Key::PartOf => head.part_of = map.next_value::<Reference>()?.0,
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

    #[test]
    fn the_head_is_what_a_full_parse_finds() {
        for text in [
            r#"{"resourceType":"Location","id":"a","meta":{"versionId":"2","lastUpdated":"2026-01-01T00:00:00Z"},"partOf":{"reference":"Location/p"}}"#,
            r#"{"id":"a","extension":[{"url":"x","valueAttachment":{"data":"..."}}],"meta":{"tag":[{"code":"c"}]}}"#,
            r#"{"id":"a\n","meta":{"lastUpdated":"2026"},"resourceType":"Patient"}"#,
            r#"{"id":5,"resourceType":["x"],"meta":"m","partOf":{"reference":{"x":1}}}"#,
            r#"{"id":null,"meta":{"lastUpdated":7},"partOf":[1,{"a":[]}]}"#,
            r#"{"id":"a","meta":{"versionId":3,"lastUpdated":"x","tag":[{"versionId":"t"}]}}"#,
            r#"{"id":"a","meta":{"versionId":"W/\"2\""},"partOf":{"versionId":"p"}}"#,
            r#"{"meta":null,"partOf":null,"n":1.5e300,"b":true}"#,
            r#"{}"#,
            r#"["a"]"#,
            r#""a""#,
            r#"7"#,
            r#"null"#,
        ] {
            let got = fast(text).unwrap();
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
            r#"{"id":"a","meta":{"versionId":"1","versionId":"2"}}"#,
            r#"{"id":"a","meta":{"versionId":"1"},"meta":{"lastUpdated":"b"}}"#,
            r#"{"partOf":{"reference":"Location/x"},"partOf":"y"}"#,
            r#"{"partOf":{"reference":"a","reference":"b"}}"#,
        ] {
            assert_eq!(fast(text).unwrap(), by_value(text).unwrap(), "{text}");
        }
        let head = fast(r#"{"resourceType":"Patient","id":"a","id":"b"}"#).unwrap();
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
            assert!(fast(text).is_err(), "{text}");
        }
        // A surrogate pair is a character.
        let pair = r#"{"id":"a","x":"\ud83d\ude00"}"#;
        assert_eq!(fast(pair).unwrap(), by_value(pair).unwrap());
    }

    #[test]
    fn a_raw_value_key_is_left_to_a_value_read() {
        let lines = [
            // The review's case: a Value read parses the string as JSON and fails.
            r#"{"resourceType":"Patient","id":"bad","x":{"$serde_json::private::RawValue":"not json"}}"#,
            r#"{"resourceType":"Patient","id":"ok","x":{"$serde_json::private::RawValue":"{\"a\":1}"}}"#,
            r#"{"$serde_json::private::RawValue":"{\"resourceType\":\"Patient\",\"id\":\"inner\"}"}"#,
            r#"{"id":"later","x":{"a":1,"$serde_json::private::RawValue":"not json"}}"#,
            r#"{"id":"esc","x":{"\u0024serde_json::private::RawValue":"not json"}}"#,
            r#"{"id":"m","meta":{"$serde_json::private::RawValue":"7"}}"#,
            r#"{"id":"s","resourceType":{"$serde_json::private::RawValue":"\"Patient\""}}"#,
            r#"{"id":"arr","x":[{"$serde_json::private::RawValue":"[]"}]}"#,
        ];
        for text in lines {
            assert!(fast(text).is_err(), "the fast reader leaves it: {text}");
            match (Head::parse(text), by_value(text)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "{text}"),
                (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{text}"),
                (a, b) => panic!("{text}: {a:?} against {b:?}"),
            }
        }
        // serde_json's `raw_value` comes on with sqlx, in a `postgres` build.
        if cfg!(feature = "postgres") {
            assert!(by_value(lines[0]).is_err(), "raw_value is on");
            assert!(Head::parse(lines[0]).is_err());
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
            assert!(fast(text).is_err(), "{text}");
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
        let heads: Vec<_> = text.lines().map(|l| fast(l).unwrap()).collect();
        let head_time = started.elapsed();
        eprintln!(
            "{} lines, {} MB: Value {full_time:?}, Head {head_time:?}",
            heads.len(),
            text.len() / 1_000_000
        );
        assert_eq!(full, heads);
    }
}
