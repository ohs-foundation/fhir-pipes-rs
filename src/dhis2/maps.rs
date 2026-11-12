//! `--maps`: write the FHIR resources with FHIR Mapping Language maps
//! instead of the built-in mapping.
//!
//! fhir-pipes still reads DHIS2 and prepares each record: the values that
//! need other records or the run's options (ids, the parent that was
//! exported, a merged boundary, parsed numbers). The record is an instance
//! of one of the logical models in examples/import/icr-dhis2/models, which
//! are built in. A map in the `--maps` folders whose first group reads that
//! model turns the record into resources. `dhis2 extract` writes the records
//! themselves, for `fhir-pipes import run` to map later.

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
    /// `dhis2 extract`: the record itself.
    Records,
}

impl<'a> Writes<'a> {
    /// Records for `dhis2 extract` (which has no `--maps`), else the map
    /// loaded from `--maps`, else the built-in mapping.
    pub fn new(map: Option<&'a RecordMap>, records: bool) -> Self {
        match (records, map) {
            (true, _) => Writes::Records,
            (false, Some(m)) => Writes::Maps(m),
            (false, None) => Writes::BuiltIn,
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
        let mut package = MapPackage::load(dirs, &MODELS)?;
        // The maps read the systems and profiles they write as parameters of
        // their import package (examples/import/icr-dhis2): its defaults.
        let (values, manifests) = crate::import::package::map_parameter_defaults(dirs)?;
        package.set_parameters(&values);
        for m in &manifests {
            eprintln!("parameters: the defaults in {}", m.display());
        }
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

/// The test cases of the example package examples/import/icr-dhis2: the
/// unit tests check that the records fhir-pipes prepares are a case's
/// `in/` file. `import test` checks that the package maps them to its
/// `expected/` files. With `FHIR_PIPES_UPDATE_SAMPLE` set, the files are
/// written instead of compared.
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
    /// in the order given within each.
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
            if update() {
                write(&path, &list);
            }
            let want = read(&path);
            assert_eq!(want.len(), list.len(), "{}", path.display());
            for (w, g) in want.iter().zip(&list) {
                assert_eq!(w, *g, "{}", path.display());
            }
        }
    }
}
