//! `--maps`: write the FHIR resources with FHIR Mapping Language maps
//! instead of the built-in mapping.
//!
//! fhir-pipes still reads DHIS2 and prepares each record: the values that
//! need other records or the run's options (ids, the parent that was
//! exported, a merged boundary, parsed numbers). The record is an instance
//! of one of the logical models in `models/`, which are built in. A map in
//! the `--maps` folders whose first group reads that model turns the record
//! into resources.

use std::path::PathBuf;

use fml_model::StructureMap;
use serde_json::Value;

use crate::error::Result;
use crate::mapping::MapPackage;
use crate::report::Report;

pub const ORG_UNIT_MODEL: &str =
    "https://healthcampaigns.org/fhir/dhis2/StructureDefinition/DHIS2OrgUnit";

pub const INDICATOR_MODEL: &str =
    "https://healthcampaigns.org/fhir/dhis2/StructureDefinition/DHIS2Indicator";
pub const VALUE_MODEL: &str =
    "https://healthcampaigns.org/fhir/dhis2/StructureDefinition/DHIS2Value";

/// The logical models of the records fhir-pipes prepares.
const MODELS: [&str; 3] = [
    include_str!("models/StructureDefinition-DHIS2OrgUnit.json"),
    include_str!("models/StructureDefinition-DHIS2Indicator.json"),
    include_str!("models/StructureDefinition-DHIS2Value.json"),
];

/// The map for one kind of record.
pub struct RecordMap {
    package: MapPackage,
    map: StructureMap,
}

impl RecordMap {
    /// The map in `dirs` that reads `model`, or `None` without `--maps`.
    pub fn load(dirs: &[PathBuf], model: &str) -> Result<Option<Self>> {
        if dirs.is_empty() {
            return Ok(None);
        }
        let package = MapPackage::load(dirs, &MODELS)?;
        let map = package.map_for_source(model)?.clone();
        eprintln!(
            "map: {} ({})",
            map.name.as_deref().unwrap_or("unnamed"),
            map.url.as_deref().unwrap_or("no url")
        );
        Ok(Some(Self { package, map }))
    }

    /// The resources the map writes for `record`. What the engine left out
    /// (a code a ConceptMap has no target for) is reported under `key`.
    pub fn run(&self, record: &Value, key: &str, report: &mut Report) -> Result<Vec<Value>> {
        let output = self.package.transform(&self.map, record)?;
        for warning in &output.warnings {
            report.add("map_warning", key, warning);
        }
        Ok(output.resources)
    }
}

/// The last path segment of a model URL: the `resourceType` of its records.
pub fn record_type(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}
