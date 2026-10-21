//! Static lint and validation support for SQL-on-FHIR ViewDefinitions.
//!
//! The FHIRPath selectors of a view are checked against FHIR element
//! information ([`ElementModel`]): unknown elements, choice elements without
//! `ofType()`, arrays and complex values in scalar columns (FH01–FH05).
//! [`lint_core`] checks against the FHIR R4 and R5 core types built into
//! fhir-pipes-sof, with no package. With the `packages` feature, [`lint`]
//! checks against a FHIR package instead: it adapts the package's
//! StructureDefinitions (via `octofhir-fhirschema`) into the analysis
//! engine's `SchemaProvider` contract, so generated SQL can also be validated
//! against the shape of a resource. Reading the package is the caller's:
//! fhir-pipes reads it as it reads every package, with `fhir::package`.

mod finding;
mod model;
#[cfg(feature = "packages")]
mod provider;
mod selectors;
mod shareable;
mod structure;
#[cfg(feature = "packages")]
mod view_lint;

pub use finding::{Finding, Severity};
pub use model::{CoreTypes, ElementKind, ElementModel};
#[cfg(feature = "packages")]
pub use provider::FhirSchemaProvider;
pub use selectors::{lint_core, lint_selectors};
pub use shareable::lint_shareable;
pub use structure::validate_structure;
#[cfg(feature = "packages")]
pub use view_lint::{lint, lint_sql, lint_view};
