//! A FHIR model for the FHIRPath engine, from a table generated from the
//! FHIR core StructureDefinitions (`fhir_types.rs`, written by
//! `examples/generate_fhir_types.rs`). The lookup functions are public for
//! other tools that need FHIR element types without a package, such as
//! `fhir-pipes-sof-lint`.
//!
//! The engine asks its `ModelProvider` for the type of each element it
//! navigates; with types it can answer `is`/`as`/`ofType()` on FHIR types
//! (`Observation.value is Quantity`, `Patient.active.is(boolean)`). The table
//! is the union of R4 and R5, so a view still runs on either version; an
//! element whose type differs between them has no type here, and the engine
//! navigates it as it does without a model. The answers follow
//! octofhir-fhirschema's provider, without its schema files or package
//! manager.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

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

/// What a FHIR type is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A resource (`Patient`), including abstract ones (`DomainResource`).
    Resource,
    /// A complex datatype (`HumanName`, `Quantity`, `BackboneElement`).
    Complex,
    /// A primitive datatype (`string`, `dateTime`).
    Primitive,
}

/// The type of an element in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementType {
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

type Row = (&'static str, &'static str, bool, ElementType);

/// The elements of a type or backbone element, sorted by name. The engine
/// asks for an element type at every step it navigates, so owners are
/// indexed once rather than searched for in all of `ELEMENTS`.
fn owned(owner: &str) -> &'static [Row] {
    static OWNERS: LazyLock<HashMap<&'static str, &'static [Row]>> = LazyLock::new(|| {
        ELEMENTS
            .chunk_by(|a, b| a.0 == b.0)
            .map(|rows| (rows[0].0, rows))
            .collect()
    });
    OWNERS.get(owner).copied().unwrap_or(&[])
}

/// The elements of a type (`Patient`) or backbone element
/// (`Patient.contact`): `(name, repeats, type)`, sorted by name. A choice
/// element is listed with its `[x]` (`value[x]`). Empty for an unknown owner.
pub fn elements(owner: &str) -> impl Iterator<Item = (&'static str, bool, &'static ElementType)> {
    owned(owner)
        .iter()
        .map(|(_, name, repeats, ty)| (*name, *repeats, ty))
}

/// One element of a type or backbone element, by its name in the table
/// (`value[x]` for a choice element): `(repeats, type)`.
pub fn element(owner: &str, name: &str) -> Option<(bool, &'static ElementType)> {
    let owned = owned(owner);
    let i = owned.binary_search_by(|(_, n, ..)| (*n).cmp(name)).ok()?;
    let (_, _, repeats, ty) = &owned[i];
    Some((*repeats, ty))
}

/// A choice element by its JSON name: `valueQuantity` is `value[x]` as a
/// Quantity, `valueString` as a string. `(repeats, type)`.
pub fn choice_by_json_name(owner: &str, name: &str) -> Option<(bool, &'static str)> {
    owned(owner).iter().find_map(|(_, element, repeats, ty)| {
        let ElementType::Choice(types) = ty else {
            return None;
        };
        // `valueQuantity` is `value` and `Quantity`.
        let suffix = name.strip_prefix(element.strip_suffix("[x]")?)?;
        // `Quantity` for `Quantity`, `DateTime` for `dateTime`.
        let ty = types.iter().find(|t| {
            let (mut ty, mut written) = (t.chars(), suffix.chars());
            ty.next().map(|c| c.to_ascii_uppercase()) == written.next()
                && ty.as_str() == written.as_str()
        })?;
        Some((*repeats, *ty))
    })
}

/// Whether `name` is a FHIR type (resource, datatype or primitive).
pub fn is_type(name: &str) -> bool {
    type_entry(name).is_some()
}

/// What the FHIR type `name` is, if the table has it.
pub fn type_kind(name: &str) -> Option<Kind> {
    type_entry(name).map(|(kind, _)| *kind)
}

/// Where the elements of `owner`'s element `name` are listed: its complex
/// type (`HumanName`) or, for a backbone element, its path (`Patient.contact`).
/// A choice element by its JSON name (`valueQuantity`) has the type it names.
/// `None` for a primitive, an unknown element, a choice element by its own
/// name, or an element the FHIR versions give different types.
pub fn element_owner(owner: &str, name: &str) -> Option<&'static str> {
    let complex = |ty: &'static str| {
        type_entry(ty)
            .is_some_and(|(kind, _)| *kind != Kind::Primitive)
            .then_some(ty)
    };
    match element(owner, name) {
        Some((_, ElementType::Named(ty))) => complex(ty),
        Some((_, ElementType::Backbone(path))) => Some(path),
        Some(_) => None,
        None => choice_by_json_name(owner, name).and_then(|(_, ty)| complex(ty)),
    }
}

/// Whether `owner` has a choice element `name` (`value[x]` for `value`).
pub fn is_choice_element(owner: &str, name: &str) -> bool {
    element(owner, &format!("{name}[x]")).is_some()
}

/// Whether any type or backbone element has a choice element `name`.
pub fn is_choice_anywhere(name: &str) -> bool {
    static NAMES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
        ELEMENTS
            .iter()
            .filter_map(|(_, name, ..)| name.strip_suffix("[x]"))
            .collect()
    });
    NAMES.contains(name)
}

fn is_derived_from(derived: &str, base: &str) -> bool {
    if derived == base {
        return true;
    }
    // A backbone element (`Patient.contact`) is a BackboneElement in a
    // resource and an Element in a datatype (`Timing.repeat`).
    if let Some((root, _)) = derived.split_once('.') {
        let parent = match type_kind(root) {
            Some(Kind::Resource) => "BackboneElement",
            _ => "Element",
        };
        return is_derived_from(parent, base);
    }
    type_entry(derived).is_some_and(|(_, bases)| bases.iter().any(|b| is_derived_from(b, base)))
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

/// `get_element_type`'s answer for each element of each owner, by the name
/// the engine asks for: the table's names, and a choice element's JSON names
/// (`valueQuantity`). Computed once: the engine asks at every step it
/// navigates, and the answer depends only on the owner and the name.
#[allow(clippy::type_complexity)]
static ELEMENT_TYPES: LazyLock<HashMap<&str, HashMap<Box<str>, TypeInfo, Fnv>, Fnv>> =
    LazyLock::new(|| {
        let mut map: HashMap<&str, HashMap<Box<str>, TypeInfo, Fnv>, Fnv> = HashMap::default();
        for (owner, name, repeats, ty) in ELEMENTS {
            let elements = map.entry(owner).or_default();
            let singleton = !repeats;
            match ty {
                ElementType::Named(ty) => {
                    if let Some(info) = named_type(ty, singleton) {
                        elements.insert((*name).into(), info);
                    }
                }
                ElementType::Backbone(path) => {
                    elements.insert((*name).into(), type_info("Any", singleton, "FHIR", path));
                }
                // `value` alone has no type: it depends on the JSON name used.
                ElementType::Choice(types) => {
                    let base = name.trim_end_matches("[x]");
                    for ty in types.iter() {
                        let mut chars = ty.chars();
                        let first = chars.next().map(|c| c.to_ascii_uppercase());
                        let json = format!("{base}{}{}", first.unwrap_or_default(), chars.as_str());
                        // An element of that name, if any, wins.
                        if let Some(info) = named_type(ty, singleton) {
                            elements.entry(json.into()).or_insert(info);
                        }
                    }
                }
                ElementType::Unknown => {}
            }
        }
        map
    });

/// FNV-1a: a fast hash for the short names the lookups use.
type Fnv = std::hash::BuildHasherDefault<FnvHasher>;

#[derive(Default)]
struct FnvHasher(u64);

impl std::hash::Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut hash = if self.0 == 0 {
            0xcbf2_9ce4_8422_2325
        } else {
            self.0
        };
        for b in bytes {
            hash = (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
        }
        self.0 = hash;
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
        Ok(ELEMENT_TYPES
            .get(owner)
            .and_then(|elements| elements.get(property_name))
            .cloned())
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
    fn elements_lead_to_their_owners() {
        assert_eq!(element_owner("Patient", "name"), Some("HumanName"));
        assert_eq!(element_owner("Patient", "contact"), Some("Patient.contact"));
        assert_eq!(element_owner("Patient.contact", "name"), Some("HumanName"));
        assert_eq!(
            element_owner("Observation", "valueQuantity"),
            Some("Quantity")
        );
        assert_eq!(
            element_owner("Questionnaire.item", "item"),
            Some("Questionnaire.item")
        );
        // A primitive, a choice element by its own name, an unknown element.
        assert_eq!(element_owner("Patient", "gender"), None);
        assert_eq!(element_owner("Observation", "value"), None);
        assert_eq!(element_owner("Patient", "nmae"), None);
    }

    #[test]
    fn choice_elements_are_found_by_owner_or_anywhere() {
        assert!(is_choice_element("Observation", "value"));
        assert!(is_choice_element("Observation.component", "value"));
        assert!(is_choice_element("Patient", "deceased"));
        assert!(!is_choice_element("Patient", "name"));
        assert!(!is_choice_element("Patient.contact", "name"));
        assert!(is_choice_anywhere("value"));
        // ImplementationGuide.definition.page.name[x] (R4).
        assert!(is_choice_anywhere("name"));
        assert!(!is_choice_anywhere("family"));
    }

    #[test]
    fn types_derive_through_their_bases() {
        let p = TableModelProvider;
        assert!(p.is_type_derived_from("Patient", "DomainResource"));
        assert!(p.is_type_derived_from("Patient", "Resource"));
        assert!(p.is_type_derived_from("Age", "Quantity"));
        assert!(!p.is_type_derived_from("Patient", "Observation"));
        assert!(p.is_type_derived_from("Patient.contact", "BackboneElement"));
        assert!(p.is_type_derived_from("Patient.contact", "Element"));
        assert!(p.is_type_derived_from("Timing.repeat", "Element"));
        assert!(!p.is_type_derived_from("Timing.repeat", "BackboneElement"));
    }
}
