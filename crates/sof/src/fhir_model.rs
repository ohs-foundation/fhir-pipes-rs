//! A FHIR model for the FHIRPath engine, from a table generated from the
//! FHIR core StructureDefinitions (`fhir_types.rs`, written by
//! `examples/generate_fhir_types.rs`).
//!
//! The engine asks its `ModelProvider` for the type of each element it
//! navigates; with types it can answer `is`/`as`/`ofType()` on FHIR types
//! (`Observation.value is Quantity`, `Patient.active.is(boolean)`). The table
//! is the union of R4 and R5, so a view still runs on either version; an
//! element whose type differs between them has no type here, and the engine
//! navigates it as it does without a model. The answers follow
//! octofhir-fhirschema's provider, without its schema files or package
//! manager.

use async_trait::async_trait;
use octofhir_fhirpath::ModelProvider;
use octofhir_fhirpath::core::model_provider::{ElementInfo, ModelError, TypeInfo};

type ModelResult<T> = std::result::Result<T, ModelError>;

include!("fhir_types.rs");

/// FHIR primitive (and Quantity-like) types and the FHIRPath type each
/// evaluates to.
const TYPE_MAPPING: &[(&str, &str)] = &[
    ("boolean", "Boolean"),
    ("integer", "Integer"),
    ("integer64", "Integer"),
    ("string", "String"),
    ("decimal", "Decimal"),
    ("uri", "String"),
    ("url", "String"),
    ("canonical", "String"),
    ("base64Binary", "String"),
    ("instant", "DateTime"),
    ("date", "Date"),
    ("dateTime", "DateTime"),
    ("time", "Time"),
    ("code", "String"),
    ("oid", "String"),
    ("id", "String"),
    ("markdown", "String"),
    ("unsignedInt", "Integer"),
    ("positiveInt", "Integer"),
    ("uuid", "String"),
    ("xhtml", "String"),
    ("Quantity", "Quantity"),
    ("SimpleQuantity", "Quantity"),
    ("MoneyQuantity", "Quantity"),
    ("Duration", "Quantity"),
    ("Age", "Quantity"),
    ("Distance", "Quantity"),
    ("Count", "Quantity"),
];

const SYSTEM_TYPES: &[&str] = &[
    "Boolean", "String", "Integer", "Decimal", "Date", "DateTime", "Time", "Quantity",
];

#[derive(Debug, PartialEq)]
enum Kind {
    Resource,
    Complex,
    Primitive,
}

#[derive(Debug)]
enum ElementType {
    /// A FHIR type, or `System.<Type>` for a FHIRPath system type.
    Named(&'static str),
    /// A choice element (`value[x]`) and the types it allows.
    Choice(&'static [&'static str]),
    /// A backbone element; its elements are listed under this path.
    Backbone(&'static str),
    /// The FHIR versions disagree on the type.
    Unknown,
}

/// The kind and base types of a FHIR type.
fn type_entry(name: &str) -> Option<(&'static Kind, &'static [&'static str])> {
    let i = TYPES.binary_search_by(|(n, ..)| (*n).cmp(name)).ok()?;
    let (_, kind, bases) = &TYPES[i];
    Some((kind, *bases))
}

/// The elements of a type or backbone element, sorted by name.
fn owned(owner: &str) -> &'static [(&'static str, &'static str, bool, ElementType)] {
    let start = ELEMENTS.partition_point(|(o, ..)| *o < owner);
    let len = ELEMENTS[start..].partition_point(|(o, ..)| *o == owner);
    &ELEMENTS[start..start + len]
}

/// The elements of a type or backbone element: `(name, repeats, type)`.
fn elements(owner: &str) -> impl Iterator<Item = (&'static str, bool, &'static ElementType)> {
    owned(owner)
        .iter()
        .map(|(_, name, repeats, ty)| (*name, *repeats, ty))
}

/// One element of a type or backbone element: `(repeats, type)`.
fn element(owner: &str, name: &str) -> Option<(bool, &'static ElementType)> {
    let owned = owned(owner);
    let i = owned.binary_search_by(|(_, n, ..)| (*n).cmp(name)).ok()?;
    let (_, _, repeats, ty) = &owned[i];
    Some((*repeats, ty))
}

fn is_derived_from(derived: &str, base: &str) -> bool {
    derived == base
        || type_entry(derived)
            .is_some_and(|(_, bases)| bases.iter().any(|b| is_derived_from(b, base)))
}

/// The FHIRPath type a FHIR type evaluates to, if it is a primitive or a
/// Quantity.
fn mapped(fhir_type: &str) -> Option<&'static str> {
    TYPE_MAPPING
        .iter()
        .find(|(f, _)| *f == fhir_type)
        .map(|(_, p)| *p)
}

fn type_info(type_name: &str, singleton: bool, namespace: &str, name: &str) -> TypeInfo {
    TypeInfo {
        type_name: type_name.to_string(),
        singleton: Some(singleton),
        is_empty: Some(false),
        namespace: Some(namespace.to_string()),
        name: Some(name.to_string()),
    }
}

/// The type of an element whose type is `ty`.
///
/// A Quantity element (Quantity, Age, Duration, ...) gets no type. The
/// engine turns a typed Quantity into a value that keeps only its value,
/// unit, code and system, and (in octofhir-fhirpath 0.4.56) navigating it
/// returns the unit for `code` and nothing for `system` (fixed by
/// octofhir/fhirpath-rs#183); its comparator, id and extensions are lost
/// either way. Untyped, it stays the resource's JSON.
///
/// A date, dateTime, instant or time element keeps its FHIR type name
/// (`is(dateTime)` works) but is a String to the engine. As a DateTime the
/// engine would write it back in its own form (`2026-01-02T03:04:05Z` as
/// `…+00:00`); comparisons and boundaries read the string as a date anyway.
fn named_type(ty: &str, singleton: bool) -> Option<TypeInfo> {
    match ty.strip_prefix("System.") {
        Some(system) => Some(type_info(system, singleton, "System", system)),
        None => match mapped(ty) {
            Some("Quantity") => None,
            Some("DateTime" | "Date" | "Time") => Some(type_info("String", singleton, "FHIR", ty)),
            fhirpath => Some(type_info(fhirpath.unwrap_or(ty), singleton, "FHIR", ty)),
        },
    }
}

/// A `ModelProvider` over the generated FHIR type table.
#[derive(Debug, Default, Clone, Copy)]
pub struct TableModelProvider;

#[async_trait]
impl ModelProvider for TableModelProvider {
    async fn get_type(&self, type_name: &str) -> ModelResult<Option<TypeInfo>> {
        if let Some((kind, _)) = type_entry(type_name) {
            let fhirpath = match (mapped(type_name), kind) {
                (Some(p), _) => p,
                (None, Kind::Primitive) => type_name,
                (None, _) => "Any",
            };
            return Ok(Some(type_info(fhirpath, true, "FHIR", type_name)));
        }
        if SYSTEM_TYPES.contains(&type_name) {
            return Ok(Some(type_info(type_name, true, "System", type_name)));
        }
        Ok(None)
    }

    async fn get_element_type(
        &self,
        parent_type: &TypeInfo,
        property_name: &str,
    ) -> ModelResult<Option<TypeInfo>> {
        let Some(owner) = parent_type.name.as_deref() else {
            return Ok(None);
        };
        if let Some((repeats, ty)) = element(owner, property_name) {
            return Ok(match ty {
                ElementType::Named(ty) => named_type(ty, !repeats),
                ElementType::Backbone(path) => Some(type_info("Any", !repeats, "FHIR", path)),
                ElementType::Choice(_) | ElementType::Unknown => None,
            });
        }
        // A choice element by its JSON name: `valueQuantity` is `value[x]`
        // as a Quantity, `valueString` as a string. `value` alone has no
        // type: it depends on the JSON name used. The type starts at an
        // upper-case letter.
        for (at, _) in property_name
            .char_indices()
            .skip(1)
            .filter(|(_, c)| c.is_ascii_uppercase())
        {
            let (base, suffix) = property_name.split_at(at);
            let Some((repeats, ElementType::Choice(types))) = element(owner, &format!("{base}[x]"))
            else {
                continue;
            };
            // `Quantity` for `Quantity`, `DateTime` for `dateTime`.
            let found = types.iter().find(|t| {
                let (mut ty, mut written) = (t.chars(), suffix.chars());
                ty.next().map(|c| c.to_ascii_uppercase()) == written.next()
                    && ty.as_str() == written.as_str()
            });
            if let Some(ty) = found {
                return Ok(named_type(ty, !repeats));
            }
        }
        Ok(None)
    }

    fn of_type(&self, type_info: &TypeInfo, target_type: &str) -> Option<TypeInfo> {
        let matches = type_info.type_name == target_type
            || type_info
                .name
                .as_deref()
                .is_some_and(|name| name == target_type || is_derived_from(name, target_type))
            || is_derived_from(&type_info.type_name, target_type);
        matches.then(|| type_info.clone())
    }

    fn get_element_names(&self, parent_type: &TypeInfo) -> Vec<String> {
        let Some(owner) = parent_type.name.as_deref() else {
            return vec![];
        };
        elements(owner).map(|(name, ..)| name.to_string()).collect()
    }

    async fn get_children_type(&self, parent_type: &TypeInfo) -> ModelResult<Option<TypeInfo>> {
        if parent_type.singleton.unwrap_or(true) {
            return Ok(None);
        }
        Ok(Some(TypeInfo {
            singleton: Some(true),
            ..parent_type.clone()
        }))
    }

    async fn get_elements(&self, type_name: &str) -> ModelResult<Vec<ElementInfo>> {
        Ok(elements(type_name)
            .map(|(name, _, ty)| ElementInfo {
                name: name.to_string(),
                element_type: match ty {
                    ElementType::Named(ty) => ty.trim_start_matches("System.").to_string(),
                    ElementType::Backbone(_) => "BackboneElement".to_string(),
                    ElementType::Choice(_) | ElementType::Unknown => "Any".to_string(),
                },
                documentation: None,
            })
            .collect())
    }

    async fn get_resource_types(&self) -> ModelResult<Vec<String>> {
        Ok(self.types_where(|kind, _| *kind == Kind::Resource))
    }

    async fn get_complex_types(&self) -> ModelResult<Vec<String>> {
        Ok(self.types_where(|kind, name| *kind != Kind::Primitive && mapped(name).is_none()))
    }

    async fn get_primitive_types(&self) -> ModelResult<Vec<String>> {
        Ok(self.types_where(|kind, _| *kind == Kind::Primitive))
    }

    fn is_type_derived_from(&self, derived_type: &str, base_type: &str) -> bool {
        is_derived_from(derived_type, base_type)
    }
}

impl TableModelProvider {
    fn types_where(&self, keep: impl Fn(&Kind, &str) -> bool) -> Vec<String> {
        TYPES
            .iter()
            .filter(|(name, kind, _)| keep(kind, name))
            .map(|(name, ..)| name.to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn element(owner: &str, name: &str) -> Option<TypeInfo> {
        let parent = type_info("Any", true, "FHIR", owner);
        TableModelProvider
            .get_element_type(&parent, name)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn elements_have_their_fhir_types() {
        let status = element("Observation", "status").await.unwrap();
        assert_eq!(status.type_name, "String");
        assert_eq!(status.name.as_deref(), Some("code"));
        let category = element("Observation", "category").await.unwrap();
        assert_eq!(category.name.as_deref(), Some("CodeableConcept"));
        assert_eq!(category.singleton, Some(false));
    }

    #[tokio::test]
    async fn a_choice_element_is_typed_by_its_json_name() {
        let value = element("Observation", "valueCodeableConcept")
            .await
            .unwrap();
        assert_eq!(value.type_name, "CodeableConcept");
        assert_eq!(value.name.as_deref(), Some("CodeableConcept"));
        let onset = element("Condition", "onsetDateTime").await.unwrap();
        assert_eq!(onset.name.as_deref(), Some("dateTime"));
        // A String to the engine, so the value is written back as it was.
        assert_eq!(onset.type_name, "String");
        assert!(element("Observation", "value").await.is_none());
        // Quantity elements stay untyped (see `named_type`).
        assert!(element("Observation", "valueQuantity").await.is_none());
        assert!(element("Condition", "onsetAge").await.is_none());
        assert!(element("Observation", "valueHumanName").await.is_none());
    }

    #[tokio::test]
    async fn backbone_elements_are_found_by_path() {
        let component = element("Observation", "component").await.unwrap();
        assert_eq!(component.name.as_deref(), Some("Observation.component"));
        let code = element("Observation.component", "code").await.unwrap();
        assert_eq!(code.name.as_deref(), Some("CodeableConcept"));
        // Questionnaire.item.item refers back to Questionnaire.item.
        let item = element("Questionnaire.item", "item").await.unwrap();
        assert_eq!(item.name.as_deref(), Some("Questionnaire.item"));
    }

    #[tokio::test]
    async fn the_table_covers_r4_and_r5() {
        // R4 only.
        assert!(
            element("MedicationRequest", "medicationReference")
                .await
                .is_some()
        );
        // R5 only.
        assert!(element("Observation", "bodyStructure").await.is_some());
        // string in R4, markdown in R5: no type.
        assert!(element("Account", "description").await.is_none());
    }

    #[test]
    fn types_derive_through_their_bases() {
        let p = TableModelProvider;
        assert!(p.is_type_derived_from("Patient", "DomainResource"));
        assert!(p.is_type_derived_from("Patient", "Resource"));
        assert!(p.is_type_derived_from("Age", "Quantity"));
        assert!(!p.is_type_derived_from("Patient", "Observation"));
    }
}
