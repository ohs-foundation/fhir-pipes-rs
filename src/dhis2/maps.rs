//! The records fhir-pipes prepares from DHIS2, and the import package that
//! turns them into FHIR resources.
//!
//! fhir-pipes reads DHIS2 and prepares each record: the values that need
//! other records or the run's options (ids, the parent that was exported, a
//! merged boundary, parsed numbers). The record is an instance of one of the
//! logical models in examples/import/icr-dhis2/models. `dhis2 extract`
//! writes the records. `dhis2 locations|measures|reports` run each record
//! through the map of an import package (`--package`) that reads its type,
//! and write the resources.

use std::collections::BTreeMap;
use std::path::Path;

use fml_model::StructureMap;
use serde_json::Value;

use crate::error::{PipesError, Result};
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
    /// `dhis2 locations|measures|reports`: the resources the package's map
    /// writes from the record.
    Maps(&'a RecordMap),
    /// `dhis2 extract`: the record itself.
    Records,
}

impl<'a> Writes<'a> {
    /// The map for `dhis2 locations|measures|reports`, or the records for
    /// `dhis2 extract`.
    pub fn new(map: Option<&'a RecordMap>) -> Self {
        match map {
            Some(m) => Writes::Maps(m),
            None => Writes::Records,
        }
    }
}

/// The map for one kind of record.
pub struct RecordMap {
    package: MapPackage,
    map: StructureMap,
    /// The value of each of the package's parameters for this run.
    values: BTreeMap<String, Value>,
    /// Threads for `transform_all`: one per core. The DHIS2 commands'
    /// `--concurrency` is for requests, not for the maps.
    threads: usize,
}

impl RecordMap {
    /// The map in the package `dir` that reads `model`, with the package's
    /// parameters set from their defaults and `params`. `dir` may also be
    /// the package's `maps/` folder, as `--maps` named it.
    pub fn load(dir: &Path, params: &[(String, String)], model: &str) -> Result<Self> {
        let loaded = crate::import::package::package_maps(dir, params).map_err(|e| {
            let m = crate::error::message(&e);
            if crate::error::exit_code(&e) == 2 {
                PipesError::Usage(m)
            } else {
                PipesError::Environment(m)
            }
        })?;
        eprintln!("package: {} ({})", loaded.name, loaded.dir.display());
        let map = loaded.maps.map_for_source(model)?.clone();
        eprintln!(
            "map: {} ({})",
            map.name.as_deref().unwrap_or("unnamed"),
            map.url.as_deref().unwrap_or("no url")
        );
        Ok(Self {
            package: loaded.maps,
            map,
            values: loaded.values,
            threads: crate::work::default_threads(),
        })
    }

    /// The value of the package's parameter `name`, when it has one and it
    /// is a string.
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.values.get(name).and_then(Value::as_str)
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

/// The example ICR package, for the unit tests.
#[cfg(test)]
pub fn icr_package(model: &str) -> RecordMap {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/import/icr-dhis2");
    RecordMap::load(&dir, &[], model).unwrap()
}

/// The last path segment of a model URL: the `resourceType` of its records.
pub fn record_type(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// The test cases of the example package examples/import/icr-dhis2: the
/// unit tests check that the records fhir-pipes prepares are a case's
/// `in/` file, and that the package maps them, as the `dhis2` commands run
/// it, to its `expected/` files. Those hold what the built-in mapping wrote
/// before it was removed, so they are only compared. With
/// `FHIR_PIPES_UPDATE_SAMPLE` set, the `in/` file is written instead.
#[cfg(test)]
pub mod case {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use serde_json::Value;

    fn dir(case: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples/import/icr-dhis2/tests")
            .join(case)
    }

    fn update() -> bool {
        std::env::var_os("FHIR_PIPES_UPDATE_SAMPLE").is_some()
    }

    fn read(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn write(path: &std::path::Path, values: &[&Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text: String = values.iter().map(|v| format!("{v}\n")).collect();
        std::fs::write(path, text).unwrap();
    }

    /// `records` are the case's `in/<their resourceType>.ndjson`.
    pub fn records(case: &str, records: &[Value]) {
        let kind = records[0]["resourceType"].as_str().unwrap();
        let path = dir(case).join("in").join(format!("{kind}.ndjson"));
        if update() {
            write(&path, &records.iter().collect::<Vec<_>>());
        }
        assert_eq!(read(&path), records, "{}", path.display());
    }

    /// `resources` are the case's `expected/` files, one per resourceType,
    /// in the order given within each. Never written.
    pub fn expected(case: &str, resources: &[Value]) {
        let mut by_type: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
        for r in resources {
            by_type
                .entry(r["resourceType"].as_str().unwrap())
                .or_default()
                .push(r);
        }
        for (kind, list) in by_type {
            let path = dir(case).join("expected").join(format!("{kind}.ndjson"));
            let want = read(&path);
            assert_eq!(want.len(), list.len(), "{}", path.display());
            for (w, g) in want.iter().zip(&list) {
                assert_eq!(w, *g, "{}", path.display());
            }
        }
    }
}
