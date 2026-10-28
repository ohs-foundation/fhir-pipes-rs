//! The artefacts built into fhir-pipes: the generic `Location` view and the
//! `LocationHierarchy` SQLView. A source cannot redefine them.

use super::resolve::Artifact;

/// The built-in artefacts, resolved before any source.
pub fn artifacts() -> Vec<Artifact> {
    Vec::new()
}
