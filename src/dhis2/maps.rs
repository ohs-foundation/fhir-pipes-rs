//! `--maps`: write the FHIR resources with FHIR Mapping Language maps
//! instead of the built-in mapping.
//!
//! fhir-pipes still reads DHIS2 and prepares each record: the values that
//! need other records or the run's options (ids, the parent that was
//! exported, a merged boundary, parsed numbers). The record is an instance
//! of one of the logical models in examples/import/icr-dhis2/models, which
//! are built in. A map in the `--maps` folders whose first group reads that
//! model turns the record into resources. With `--records` the records
//! themselves are written, for `fhir-pipes import run` to map later.

use std::path::PathBuf;

use fml_model::StructureMap;
use serde_json::Value;

use crate::error::Result;
use crate::mapping::{MapPackage, Output};
use crate::report::Report;

pub const ORG_UNIT_MODEL: &str =
    "https://healthcampaigns.org/fhir/dhis2/StructureDefinition/DHIS2OrgUnit";

pub const INDICATOR_MODEL: &str =
    "https://healthcampaigns.org/fhir/dhis2/StructureDefinition/DHIS2Indicator";
pub const VALUE_MODEL: &str =
    "https://healthcampaigns.org/fhir/dhis2/StructureDefinition/DHIS2Value";

/// The logical models of the records fhir-pipes prepares.
pub const MODELS: [&str; 3] = [
    include_str!("../../examples/import/icr-dhis2/models/StructureDefinition-DHIS2OrgUnit.json"),
    include_str!("../../examples/import/icr-dhis2/models/StructureDefinition-DHIS2Indicator.json"),
    include_str!("../../examples/import/icr-dhis2/models/StructureDefinition-DHIS2Value.json"),
];

/// What a DHIS2 command writes for each record it prepares.
#[derive(Clone, Copy)]
pub enum Writes<'a> {
    /// The resources of the built-in mapping.
    BuiltIn,
    /// `--maps`: the resources the map writes from the record.
    Maps(&'a RecordMap),
    /// `--records`: the record itself.
    Records,
}

impl<'a> Writes<'a> {
    /// `--records`, `--maps` (a loaded map) or neither. clap refuses both.
    pub fn new(map: Option<&'a RecordMap>, records: bool) -> Self {
        match (map, records) {
            (_, true) => Writes::Records,
            (Some(m), false) => Writes::Maps(m),
            (None, false) => Writes::BuiltIn,
        }
    }

    /// Whether the command prepares records rather than the built-in
    /// resources.
    pub fn prepares_records(self) -> bool {
        !matches!(self, Writes::BuiltIn)
    }
}

/// The map for one kind of record.
pub struct RecordMap {
    package: MapPackage,
    map: StructureMap,
    /// Threads for `transform_all`: one per core. The DHIS2 commands'
    /// `--concurrency` is for requests, not for the maps.
    threads: usize,
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
        Ok(Some(Self {
            package,
            map,
            threads: crate::work::default_threads(),
        }))
    }

    /// The resources the map writes for `record`. What the engine left out
    /// (a code a ConceptMap has no target for) is reported under `key`.
    #[cfg(test)]
    pub fn run(&self, record: &Value, key: &str, report: &mut Report) -> Result<Vec<Value>> {
        Self::take(self.package.transform(&self.map, record), key, report)
    }

    /// The map's outputs for `records`, transformed on several threads, in
    /// record order. Pass each to `take`, in order, so the warnings reach
    /// the run report in the order `run` would add them.
    pub fn transform_all(&self, records: &[&Value]) -> Vec<Result<Output>> {
        self.package.transform_all(&self.map, records, self.threads)
    }

    /// The resources of one output of `transform_all`, after reporting its
    /// warnings under `key`.
    pub fn take(output: Result<Output>, key: &str, report: &mut Report) -> Result<Vec<Value>> {
        let output = output?;
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
