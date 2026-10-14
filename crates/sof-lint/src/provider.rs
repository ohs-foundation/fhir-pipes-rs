//! Adapter from `octofhir_fhirschema::FhirSchema` to
//! [`banshee_hir::SchemaProvider`].

use std::collections::HashMap;

use banshee_hir::{ColumnInfo, DataType, SchemaProvider, TableInfo, TableType};
use octofhir_canonical_manager::CanonicalManager;
use octofhir_fhirschema::{FhirSchema, FhirSchemaElement, StructureDefinition, translate};

use crate::LintError;

/// A [`SchemaProvider`] backed by FHIR schemas.
///
/// FHIR resources are stored in a single JSONB `resource` column, so every
/// resource table exposes the same physical columns (`id`, `resource`,
/// `resource_type`, `status`); the interesting validation happens inside the
/// `resource` column via the JSONB methods, which walk the `FhirSchema`
/// element tree to answer "does this field exist here?" and "is it an array?".
#[derive(Debug, Default, Clone)]
pub struct FhirSchemaProvider {
    /// Every known schema (resources and complex types), keyed by FHIR type
    /// name (e.g. `Patient`, `HumanName`, `Observation`).
    schemas: HashMap<String, FhirSchema>,

    /// Lower-cased table name → resource type name, for the resource-kind
    /// schemas that can back a SQL table.
    tables: HashMap<String, String>,
}

impl FhirSchemaProvider {
    /// Create an empty provider.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a provider from a set of schemas.
    pub fn with_schemas(schemas: impl IntoIterator<Item = FhirSchema>) -> Self {
        let mut provider = Self::default();
        for schema in schemas {
            provider.insert(schema);
        }
        provider
    }

    /// Add a single schema. Resource-kind schemas also register a SQL table.
    pub fn insert(&mut self, schema: FhirSchema) {
        if schema.kind == "resource" {
            self.tables
                .insert(schema.type_name.to_lowercase(), schema.type_name.clone());
        }
        self.schemas.insert(schema.type_name.clone(), schema);
    }

    /// Load every `StructureDefinition` of a package already present in the
    /// canonical-manager store and adapt them into schemas.
    pub async fn from_package(
        manager: &CanonicalManager,
        package_name: &str,
    ) -> Result<Self, LintError> {
        let result = manager
            .search()
            .await
            .resource_type("StructureDefinition")
            .package(package_name)
            .limit(1_000_000)
            .execute()
            .await
            .map_err(|e| LintError::PackageLoad(e.to_string()))?;

        let mut provider = Self::default();
        for matched in result.resources {
            let Ok(sd) = serde_json::from_value::<StructureDefinition>(matched.resource.content)
            else {
                continue;
            };
            if let Ok(schema) = translate(sd, None) {
                provider.insert(schema);
            }
        }
        Ok(provider)
    }

    /// Initialise a default canonical-manager and load a package, installing it
    /// first when a `version` is given (offline-only when it is omitted).
    pub async fn load(package_name: &str, version: Option<&str>) -> Result<Self, LintError> {
        let manager = CanonicalManager::with_default_config()
            .await
            .map_err(|e| LintError::PackageLoad(e.to_string()))?;
        if let Some(version) = version {
            manager
                .install_package(package_name, version)
                .await
                .map_err(|e| LintError::PackageLoad(e.to_string()))?;
        }
        Self::from_package(&manager, package_name).await
    }

    /// True when no resource tables are known (e.g. the package was not found).
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    fn elements_for_type(&self, ty: &str) -> Option<&HashMap<String, FhirSchemaElement>> {
        self.schemas.get(ty)?.elements.as_ref()
    }

    /// Children of an element: inline backbone children, or, failing that, the
    /// elements of the named type the element points at.
    fn child_elements<'a>(
        &'a self,
        el: &'a FhirSchemaElement,
    ) -> Option<&'a HashMap<String, FhirSchemaElement>> {
        if let Some(nested) = el.elements.as_ref() {
            return Some(nested);
        }
        self.elements_for_type(el.type_name.as_deref()?)
    }

    /// The element reached by walking `path` from a root resource type.
    fn element_at(&self, root_type: &str, path: &[&str]) -> Option<&FhirSchemaElement> {
        let mut elements = self.elements_for_type(root_type)?;
        for (i, key) in path.iter().enumerate() {
            let el = elements.get(*key)?;
            if i + 1 == path.len() {
                return Some(el);
            }
            elements = self.child_elements(el)?;
        }
        None
    }

    /// Whether the element reached by `path` is a polymorphic choice element
    /// (`value[x]` and friends), i.e. it must be narrowed with `ofType()` before
    /// a concrete value can be extracted.
    pub fn is_choice_at(&self, root_type: &str, path: &[&str]) -> bool {
        self.element_at(root_type, path)
            .is_some_and(|el| el.choices.is_some())
    }

    /// Classify the element reached by `path` as a primitive value, a
    /// `Reference`, or any other complex/backbone element. `None` when no
    /// element resolves there.
    pub(crate) fn element_kind_at(&self, root_type: &str, path: &[&str]) -> Option<ElementKind> {
        let el = self.element_at(root_type, path)?;
        Some(match el.type_name.as_deref() {
            Some("Reference") => ElementKind::Reference,
            Some(t) if is_fhir_primitive(t) => ElementKind::Primitive,
            _ => ElementKind::Complex,
        })
    }

    /// The set of elements available *at* `path` (i.e. its children, or the
    /// resource's top-level elements when `path` is empty).
    fn elements_at(
        &self,
        root_type: &str,
        path: &[&str],
    ) -> Option<&HashMap<String, FhirSchemaElement>> {
        if path.is_empty() {
            return self.elements_for_type(root_type);
        }
        self.child_elements(self.element_at(root_type, path)?)
    }
}

impl SchemaProvider for FhirSchemaProvider {
    fn lookup_table(&self, _schema: Option<&str>, name: &str) -> Option<TableInfo> {
        self.tables.get(&name.to_lowercase()).map(|_| TableInfo {
            schema: None,
            name: name.to_string(),
            table_type: TableType::Table,
        })
    }

    fn lookup_columns(&self, _schema: Option<&str>, table: &str) -> Vec<ColumnInfo> {
        if !self.tables.contains_key(&table.to_lowercase()) {
            return Vec::new();
        }
        vec![
            col("id", DataType::Text, false, 0),
            col("resource", DataType::Jsonb, false, 1),
            col("resource_type", DataType::Text, true, 2),
            col("status", DataType::Text, true, 3),
        ]
    }

    fn all_table_names(&self) -> Vec<String> {
        self.tables.keys().cloned().collect()
    }

    fn lookup_jsonb_fields(
        &self,
        _schema: Option<&str>,
        table: &str,
        column: &str,
        path: &[&str],
    ) -> Option<Vec<String>> {
        if column != "resource" {
            return None;
        }
        let root = self.tables.get(&table.to_lowercase())?;
        let elements = self.elements_at(root, path)?;
        Some(elements.keys().cloned().collect())
    }

    fn jsonb_field_is_array(
        &self,
        _schema: Option<&str>,
        table: &str,
        column: &str,
        path: &[&str],
    ) -> Option<bool> {
        if column != "resource" {
            return None;
        }
        let root = self.tables.get(&table.to_lowercase())?;
        self.element_at(root, path)?.array
    }
}

/// The shape of a resolved FHIR element, for column-selector validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElementKind {
    /// A FHIR primitive (string, code, dateTime, …) — a tabular-friendly value.
    Primitive,
    /// A `Reference` — needs `getReferenceKey()` to yield a usable key.
    Reference,
    /// Any other complex type or inline backbone element.
    Complex,
}

/// FHIR primitive data types (spec "Primitive Types"), the values a flat column
/// can hold directly.
fn is_fhir_primitive(type_name: &str) -> bool {
    matches!(
        type_name,
        "base64Binary"
            | "boolean"
            | "canonical"
            | "code"
            | "date"
            | "dateTime"
            | "decimal"
            | "id"
            | "instant"
            | "integer"
            | "integer64"
            | "markdown"
            | "oid"
            | "positiveInt"
            | "string"
            | "time"
            | "unsignedInt"
            | "uri"
            | "url"
            | "uuid"
            | "xhtml"
    )
}

fn col(name: &str, data_type: DataType, not_null: bool, ordinal: usize) -> ColumnInfo {
    ColumnInfo {
        name: name.to_string(),
        data_type,
        nullable: !not_null,
        ordinal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider() -> FhirSchemaProvider {
        let patient: FhirSchema = serde_json::from_value(json!({
            "url": "http://hl7.org/fhir/StructureDefinition/Patient",
            "name": "Patient",
            "type": "Patient",
            "kind": "resource",
            "class": "resource",
            "elements": {
                "gender": { "type": "code" },
                "name": { "type": "HumanName", "array": true },
                "birthDate": { "type": "date" }
            }
        }))
        .unwrap();
        let human_name: FhirSchema = serde_json::from_value(json!({
            "url": "http://hl7.org/fhir/StructureDefinition/HumanName",
            "name": "HumanName",
            "type": "HumanName",
            "kind": "complex-type",
            "class": "complex-type",
            "elements": {
                "family": { "type": "string" },
                "given": { "type": "string", "array": true }
            }
        }))
        .unwrap();
        FhirSchemaProvider::with_schemas([patient, human_name])
    }

    #[test]
    fn resource_table_resolves() {
        let p = provider();
        assert!(p.lookup_table(None, "patient").is_some());
        assert!(p.lookup_table(None, "Patient").is_some());
        assert!(p.lookup_table(None, "observation").is_none());
        assert_eq!(p.lookup_columns(None, "patient").len(), 4);
    }

    #[test]
    fn top_level_fields() {
        let p = provider();
        let mut fields = p
            .lookup_jsonb_fields(None, "patient", "resource", &[])
            .unwrap();
        fields.sort();
        assert_eq!(fields, vec!["birthDate", "gender", "name"]);
    }

    #[test]
    fn nested_type_fields() {
        let p = provider();
        let mut fields = p
            .lookup_jsonb_fields(None, "patient", "resource", &["name"])
            .unwrap();
        fields.sort();
        assert_eq!(fields, vec!["family", "given"]);
    }

    #[test]
    fn array_cardinality() {
        let p = provider();
        assert_eq!(
            p.jsonb_field_is_array(None, "patient", "resource", &["name"]),
            Some(true)
        );
        assert_eq!(
            p.jsonb_field_is_array(None, "patient", "resource", &["gender"]),
            None
        );
        assert_eq!(
            p.jsonb_field_is_array(None, "patient", "resource", &["name", "given"]),
            Some(true)
        );
        assert_eq!(
            p.jsonb_field_is_array(None, "patient", "resource", &["name", "family"]),
            None
        );
    }

    #[test]
    fn unknown_field_is_none() {
        let p = provider();
        assert!(
            p.lookup_jsonb_fields(None, "patient", "resource", &["nope"])
                .is_none()
        );
        // non-resource column is not described
        assert!(p.lookup_jsonb_fields(None, "patient", "id", &[]).is_none());
    }
}
