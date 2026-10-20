//! The FHIR element information the selector checks need: from a FHIR
//! package ([`FhirSchemaProvider`](crate::FhirSchemaProvider), with the
//! `packages` feature), or from the R4 and R5 core types built into
//! fhir-pipes-sof ([`CoreTypes`]), which needs no package.

use fhir_pipes_sof::fhir_model::{self, ElementType, Kind};

/// The shape of a resolved FHIR element, for column-selector validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementKind {
    /// A FHIR primitive (string, code, dateTime, …) — a tabular-friendly value.
    Primitive,
    /// A `Reference` — needs `getReferenceKey()` to yield a usable key.
    Reference,
    /// Any other complex type or inline backbone element.
    Complex,
}

/// FHIR element information for the selector checks. `root` is a resource
/// type and `path` the element names from it (`["name", "family"]`).
pub trait ElementModel {
    /// The names that can follow `path`: the elements of what it reaches, or
    /// the resource's when `path` is empty. A choice element is listed by its
    /// own name (`value`) and by each of its JSON names (`valueQuantity`).
    /// `None` when `path` does not resolve.
    fn element_names(&self, root: &str, path: &[&str]) -> Option<Vec<String>>;

    /// Whether the element at `path` repeats; `None` when not known.
    fn is_array(&self, root: &str, path: &[&str]) -> Option<bool>;

    /// Whether the element at `path` is a choice element by its own name
    /// (`value`), which must be narrowed with `ofType()`.
    fn is_choice(&self, root: &str, path: &[&str]) -> bool;

    /// What the element at `path` holds; `None` when not known.
    fn kind(&self, root: &str, path: &[&str]) -> Option<ElementKind>;
}

/// The FHIR R4 and R5 core types built into fhir-pipes-sof, as one model (see
/// its `fhir_model`). An element whose type differs between the versions is
/// known to exist, but not what it holds.
#[derive(Debug, Default, Clone, Copy)]
pub struct CoreTypes;

impl CoreTypes {
    /// The type or backbone element whose elements follow `path`.
    fn owner(root: &str, path: &[&str]) -> Option<String> {
        fhir_model::is_type(root).then_some(())?;
        let mut owner = root.to_string();
        for step in path {
            owner = fhir_model::element_owner(&owner, step)?.to_string();
        }
        Some(owner)
    }

    /// The last element of `path`: whether it repeats and its type, by its
    /// table name or, for a choice element, its JSON name.
    fn last(root: &str, path: &[&str]) -> Option<(bool, Option<&'static str>, bool)> {
        let (name, parent) = path.split_last()?;
        let owner = Self::owner(root, parent)?;
        // (repeats, type when known, is a choice element by its own name)
        match fhir_model::element(&owner, name) {
            Some((repeats, ElementType::Named(ty))) => Some((repeats, Some(ty), false)),
            Some((repeats, ElementType::Backbone(_))) => {
                Some((repeats, Some("BackboneElement"), false))
            }
            Some((repeats, _)) => Some((repeats, None, false)),
            None => match fhir_model::element(&owner, &format!("{name}[x]")) {
                Some((repeats, _)) => Some((repeats, None, true)),
                None => fhir_model::choice_by_json_name(&owner, name)
                    .map(|(repeats, ty)| (repeats, Some(ty), false)),
            },
        }
    }
}

impl ElementModel for CoreTypes {
    fn element_names(&self, root: &str, path: &[&str]) -> Option<Vec<String>> {
        let owner = Self::owner(root, path)?;
        let mut names = Vec::new();
        for (name, _, ty) in fhir_model::elements(&owner) {
            match (name.strip_suffix("[x]"), ty) {
                (Some(base), ElementType::Choice(types)) => {
                    names.push(base.to_string());
                    for t in types.iter() {
                        let mut chars = t.chars();
                        let first = chars.next().map(|c| c.to_ascii_uppercase());
                        names.push(format!(
                            "{base}{}{}",
                            first.unwrap_or_default(),
                            chars.as_str()
                        ));
                    }
                }
                _ => names.push(name.to_string()),
            }
        }
        (!names.is_empty()).then_some(names)
    }

    fn is_array(&self, root: &str, path: &[&str]) -> Option<bool> {
        Self::last(root, path).map(|(repeats, ..)| repeats)
    }

    fn is_choice(&self, root: &str, path: &[&str]) -> bool {
        Self::last(root, path).is_some_and(|(.., choice)| choice)
    }

    fn kind(&self, root: &str, path: &[&str]) -> Option<ElementKind> {
        let (_, ty, _) = Self::last(root, path)?;
        Some(match ty? {
            "Reference" => ElementKind::Reference,
            ty if ty.starts_with("System.")
                || fhir_model::type_kind(ty) == Some(Kind::Primitive) =>
            {
                ElementKind::Primitive
            }
            _ => ElementKind::Complex,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_types_answer_for_elements() {
        let m = CoreTypes;
        let names = m.element_names("Patient", &[]).unwrap();
        assert!(names.contains(&"gender".to_string()));
        assert!(names.contains(&"deceased".to_string()));
        assert!(names.contains(&"deceasedBoolean".to_string()));
        assert!(
            m.element_names("Patient", &["contact"])
                .unwrap()
                .contains(&"relationship".to_string())
        );
        assert_eq!(m.is_array("Patient", &["name"]), Some(true));
        assert_eq!(m.is_array("Patient", &["gender"]), Some(false));
        assert!(m.is_choice("Observation", &["value"]));
        assert!(!m.is_choice("Observation", &["valueQuantity"]));
        assert_eq!(m.kind("Patient", &["gender"]), Some(ElementKind::Primitive));
        assert_eq!(m.kind("Patient", &["id"]), None, "string in R4, id in R5");
        assert_eq!(
            m.kind("Patient", &["managingOrganization"]),
            Some(ElementKind::Reference)
        );
        assert_eq!(m.kind("Patient", &["name"]), Some(ElementKind::Complex));
        assert_eq!(m.kind("Patient", &["contact"]), Some(ElementKind::Complex));
        assert_eq!(
            m.kind("Observation", &["valueQuantity"]),
            Some(ElementKind::Complex)
        );
        assert_eq!(
            m.kind("Patient", &["name", "family"]),
            Some(ElementKind::Primitive)
        );
        assert_eq!(m.element_names("NotAType", &[]), None);
    }
}
