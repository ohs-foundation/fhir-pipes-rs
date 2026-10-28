//! The artefacts built into fhir-pipes: the generic `Location` view and the
//! `LocationHierarchy` SQLView. A source cannot redefine them. Their files
//! are in `views/location/`.

use super::Library;
use super::resolve::Artifact;
use super::source::build_from;
use crate::views::run::{View, parse_view, table_name};

const LOCATION_VIEW: &str = include_str!("../../views/location/Location.json");
const HIERARCHY_SQL: &str = include_str!("../../views/location/LocationHierarchy.sql");
const HIERARCHY_META: &str = include_str!("../../views/location/LocationHierarchy.toml");

/// The built-in artefacts, resolved before any source.
pub fn artifacts() -> Vec<Artifact> {
    // The files are part of the binary and tested, so a failure here is a
    // build mistake, not an operator's.
    let (json, definition) = parse_view(LOCATION_VIEW)
        .expect("views/location/Location.json parses")
        .expect("views/location/Location.json is a ViewDefinition");
    let name = definition
        .name
        .clone()
        .expect("the Location view has a name");
    let location = View {
        table: table_name(&name, &[]),
        resource: definition.resource.clone(),
        name,
        json,
        definition,
    };
    let hierarchy = build_from(HIERARCHY_SQL, &[], HIERARCHY_META)
        .and_then(|v| Library::from_json(&v))
        .expect("views/location/LocationHierarchy builds");
    vec![Artifact::View(location), Artifact::Library(hierarchy)]
}
