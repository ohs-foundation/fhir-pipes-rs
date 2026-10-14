//! Static lint and validation support for SQL-on-FHIR ViewDefinitions.
//!
//! This crate adapts the FHIR schema (via `octofhir-fhirschema` and
//! `octofhir-canonical-manager`) into the analysis engine's
//! [`banshee_hir::SchemaProvider`] contract, so that generated SQL can be
//! validated against the real shape of a resource: which JSONB fields exist at
//! each path and whether they are arrays.
//!
//! The provider is the schema source for the FHIR lint pack. It is offline once
//! the FHIR package is present in the canonical-manager store.

mod finding;
mod provider;
mod shareable;
mod structure;
mod view_lint;

pub use finding::{Finding, Severity};
pub use provider::FhirSchemaProvider;
pub use shareable::lint_shareable;
pub use structure::validate_structure;
pub use view_lint::{lint, lint_sql, lint_view};

/// Errors raised while loading a FHIR package into a [`FhirSchemaProvider`].
#[derive(Debug, thiserror::Error)]
pub enum LintError {
    /// The canonical-manager could not be initialised or the package failed to
    /// load.
    #[error("package load error: {0}")]
    PackageLoad(String),
}
