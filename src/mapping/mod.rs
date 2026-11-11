//! FHIR Mapping Language maps: load a folder of maps, the logical models
//! they read and the ConceptMaps they translate with, and run a map over a
//! record.
//!
//! A map folder holds `*.map` (or `*.fml`) files and FHIR JSON:
//! StructureDefinitions (logical models, profiles, extensions), ConceptMaps
//! and compiled StructureMaps. Subfolders are read too. The FHIR R4 core
//! definitions are built in.
//!
//! The engine is `fml-engine`. Its FHIRPath comes from the engine the views
//! use (crates/sof), so maps and views evaluate expressions the same way.

mod fhirpath;

use std::path::{Path, PathBuf};

use fml_check::{CoreDefinitions, PackageDefinitions, Release};
use fml_engine::{ConceptMapSet, Engine, MapSet};
use fml_model::StructureMap;
use fml_syntax::{Diagnostic, Severity};
use serde_json::Value;

use crate::error::{PipesError, Result};
use fhirpath::FhirPath;

pub mod cli;

/// A map with the file it came from.
struct Loaded {
    path: PathBuf,
    map: StructureMap,
}

/// What a map produced from one record.
#[derive(Debug, Default)]
pub struct Output {
    /// The resources: the entries of a Bundle target, or the target itself,
    /// then anything the map created outside its target.
    pub resources: Vec<Value>,
    /// Values the engine left out, such as a code a ConceptMap has no
    /// target for.
    pub warnings: Vec<String>,
}

/// The maps, definitions and ConceptMaps of one or more folders.
pub struct MapPackage {
    definitions: PackageDefinitions<CoreDefinitions>,
    maps: Vec<Loaded>,
    imports: MapSet,
    concept_maps: ConceptMapSet,
    expressions: FhirPath,
    /// FML sources, kept for `check`.
    sources: Vec<(PathBuf, String)>,
}

fn usage(message: String) -> PipesError {
    PipesError::Usage(message)
}

/// `file:line:column: message` for a diagnostic in `text`.
fn located(path: &Path, text: &str, d: &Diagnostic) -> String {
    let offset = usize::from(d.range.start()).min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    format!("{}:{line}:{column}: {}", path.display(), d.message)
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| PipesError::io(dir, e))?;
    for entry in entries {
        let path = entry.map_err(|e| PipesError::io(dir, e))?.path();
        let hidden = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'));
        if hidden {
            continue;
        }
        if path.is_dir() {
            files_under(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

impl MapPackage {
    /// Loads every folder in `dirs`, after the StructureDefinitions in
    /// `built_in` (JSON text). A folder's copy of a built-in one is not used.
    pub fn load(dirs: &[PathBuf], built_in: &[&str]) -> Result<Self> {
        let mut definitions = PackageDefinitions::new(CoreDefinitions::new(Release::R4));
        for text in built_in {
            let resource: Value = serde_json::from_str(text)?;
            definitions
                .add(&resource)
                .map_err(|e| PipesError::Environment(format!("built-in definition: {e}")))?;
        }
        let built_in_urls: Vec<String> = built_in
            .iter()
            .filter_map(|t| serde_json::from_str::<Value>(t).ok())
            .filter_map(|v| v.get("url").and_then(Value::as_str).map(str::to_string))
            .collect();

        let mut files = Vec::new();
        for dir in dirs {
            if !dir.is_dir() {
                return Err(usage(format!("{} is not a folder of maps", dir.display())));
            }
            files_under(dir, &mut files)?;
        }
        files.sort();

        let mut maps = Vec::new();
        let mut imports = MapSet::new();
        let mut concept_maps = ConceptMapSet::new();
        let mut sources = Vec::new();
        for path in files {
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            match extension {
                "map" | "fml" => {
                    let text =
                        std::fs::read_to_string(&path).map_err(|e| PipesError::io(&path, e))?;
                    let compiled = fml_model::compile(&text);
                    let errors: Vec<String> = compiled
                        .diagnostics
                        .iter()
                        .filter(|d| d.severity == Severity::Error)
                        .map(|d| located(&path, &text, d))
                        .collect();
                    if !errors.is_empty() {
                        return Err(usage(errors.join("\n")));
                    }
                    imports.add(compiled.map.clone());
                    maps.push(Loaded {
                        path: path.clone(),
                        map: compiled.map,
                    });
                    sources.push((path, text));
                }
                "json" => {
                    let text =
                        std::fs::read_to_string(&path).map_err(|e| PipesError::io(&path, e))?;
                    let resource: Value = serde_json::from_str(&text)
                        .map_err(|e| usage(format!("{}: {e}", path.display())))?;
                    match resource.get("resourceType").and_then(Value::as_str) {
                        Some("StructureDefinition") => {
                            let url = resource.get("url").and_then(Value::as_str).unwrap_or("");
                            if built_in_urls.iter().any(|u| u == url) {
                                eprintln!(
                                    "{}: {url} is built into fhir-pipes; this copy is not used",
                                    path.display()
                                );
                                continue;
                            }
                            definitions
                                .add(&resource)
                                .map_err(|e| usage(format!("{}: {e}", path.display())))?;
                        }
                        Some("ConceptMap") => {
                            concept_maps.add(resource);
                        }
                        Some("StructureMap") => {
                            let map = fml_model::from_json(resource)
                                .map_err(|e| usage(format!("{}: {e}", path.display())))?;
                            imports.add(map.clone());
                            maps.push(Loaded { path, map });
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        Ok(Self {
            definitions,
            maps,
            imports,
            concept_maps,
            expressions: FhirPath::new()?,
            sources,
        })
    }

    fn describe(&self, loaded: &Loaded) -> String {
        format!(
            "{} ({})",
            loaded.map.name.as_deref().unwrap_or("unnamed"),
            loaded.path.display()
        )
    }

    /// The one map whose first group reads `model`, a StructureDefinition
    /// URL. None or several is a usage error.
    pub fn map_for_source(&self, model: &str) -> Result<&StructureMap> {
        let found: Vec<&Loaded> = self
            .maps
            .iter()
            .filter(|l| first_source_url(&l.map).as_deref() == Some(model))
            .collect();
        match found.as_slice() {
            [one] => Ok(&one.map),
            [] => Err(usage(format!(
                "no map reads {model}: the first group's source must be that type"
            ))),
            several => Err(usage(format!(
                "more than one map reads {model}: {}",
                several
                    .iter()
                    .map(|l| self.describe(l))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// The map with this name or canonical URL.
    pub fn map_named(&self, name: &str) -> Result<&StructureMap> {
        let found: Vec<&Loaded> = self
            .maps
            .iter()
            .filter(|l| l.map.name.as_deref() == Some(name) || l.map.url.as_deref() == Some(name))
            .collect();
        match found.as_slice() {
            [one] => Ok(&one.map),
            [] => Err(usage(format!(
                "no map is named {name}; the maps are: {}",
                self.maps
                    .iter()
                    .map(|l| self.describe(l))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
            _ => Err(usage(format!("more than one map is named {name}"))),
        }
    }

    /// The only map, when there is one.
    pub fn only_map(&self) -> Result<&StructureMap> {
        match self.maps.as_slice() {
            [one] => Ok(&one.map),
            [] => Err(usage("the folder holds no maps".into())),
            _ => Err(usage(format!(
                "the folder holds several maps; choose one with --map: {}",
                self.maps
                    .iter()
                    .map(|l| self.describe(l))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// Runs `map` over one record.
    pub fn transform(&self, map: &StructureMap, source: &Value) -> Result<Output> {
        let engine = Engine::new(&self.definitions, &self.expressions)
            .with_maps(&self.imports)
            .with_concept_maps(&self.concept_maps);
        let transformed = crate::runtime::block_on(engine.transform(map, source))
            .map_err(|e| PipesError::Environment(format!("starting the map runtime: {e}")))?
            .map_err(|e| {
                PipesError::Environment(format!(
                    "map {}: {e}",
                    map.name.as_deref().unwrap_or("unnamed")
                ))
            })?;
        let mut resources = Vec::new();
        let target = transformed.target;
        if target.get("resourceType").and_then(Value::as_str) == Some("Bundle") {
            for entry in target
                .get("entry")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(resource) = entry.get("resource") {
                    resources.push(resource.clone());
                }
            }
        } else {
            resources.push(target);
        }
        resources.extend(transformed.produced);
        Ok(Output {
            resources,
            warnings: transformed.warnings,
        })
    }

    /// Syntax and type problems of every FML file, as `file:line:column:
    /// message` lines: errors, then warnings.
    pub fn check(&self) -> (Vec<String>, Vec<String>) {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        for (path, text) in &self.sources {
            let parse = fml_syntax::parse(text);
            let mut all: Vec<Diagnostic> = parse.diagnostics().to_vec();
            all.extend(fml_check::check(&parse.tree(), &self.definitions));
            for d in all {
                let line = located(path, text, &d);
                match d.severity {
                    Severity::Error => errors.push(line),
                    _ => warnings.push(line),
                }
            }
        }
        (errors, warnings)
    }

    pub fn map_count(&self) -> usize {
        self.maps.len()
    }
}

/// The URL of the type the map's first group reads.
fn first_source_url(map: &StructureMap) -> Option<String> {
    let group = map.group.first()?;
    let input = group.input.iter().find(|i| i.mode == "source")?;
    let type_name = input.type_name.as_deref()?;
    map.structure
        .iter()
        .filter(|s| s.mode == "source")
        .find(|s| {
            s.alias.as_deref() == Some(type_name)
                || s.url == type_name
                || s.url.rsplit('/').next() == Some(type_name)
        })
        .map(|s| s.url.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dhis2::maps::{INDICATOR_MODEL, MODELS, ORG_UNIT_MODEL, VALUE_MODEL};
    use serde_json::json;

    const FACILITY: &str = r#"{"resourceType":"StructureDefinition","id":"Facility",
        "url":"https://example.org/StructureDefinition/Facility","name":"Facility","status":"draft",
        "fhirVersion":"4.0.1","kind":"logical","abstract":false,
        "type":"https://example.org/StructureDefinition/Facility",
        "baseDefinition":"http://hl7.org/fhir/StructureDefinition/Element","derivation":"specialization",
        "snapshot":{"element":[
          {"id":"Facility","path":"Facility","min":0,"max":"*"},
          {"id":"Facility.code","path":"Facility.code","min":1,"max":"1","type":[{"code":"string"}]},
          {"id":"Facility.name","path":"Facility.name","min":1,"max":"1","type":[{"code":"string"}]},
          {"id":"Facility.kind","path":"Facility.kind","min":0,"max":"1","type":[{"code":"code"}]}]}}"#;

    const KINDS: &str = r#"{"resourceType":"ConceptMap","url":"https://example.org/ConceptMap/kinds","status":"draft",
        "group":[{"source":"https://example.org/kinds","target":"http://terminology.hl7.org/CodeSystem/location-physical-type",
          "element":[{"code":"site","target":[{"code":"si","display":"Site","equivalence":"equivalent"}]}]}]}"#;

    fn map(name: &str, body: &str) -> String {
        format!(
            "/// url = 'https://example.org/StructureMap/{name}'\n/// name = '{name}'\n\n\
             uses \"https://example.org/StructureDefinition/Facility\" alias Facility as source\n\
             uses \"http://hl7.org/fhir/StructureDefinition/Location\" alias Location as target\n\n\
             group Facility(source src : Facility, target loc : Location) {{\n{body}}}\n"
        )
    }

    fn folder(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn load(dir: &tempfile::TempDir) -> Result<MapPackage> {
        MapPackage::load(&[dir.path().to_path_buf()], &[])
    }

    fn message<T>(result: Result<T>) -> String {
        match result {
            Err(PipesError::Usage(m)) => m,
            Err(other) => panic!("expected a usage error, got {other}"),
            Ok(_) => panic!("expected a usage error"),
        }
    }

    #[test]
    fn a_map_turns_a_logical_model_record_into_a_resource() {
        let body = "  src.code as c -> loc.id = c \"id\";\n  src.name as n -> loc.name = n \"name\";\n  \
                    src.kind as k -> loc.physicalType = translate(k, 'https://example.org/ConceptMap/kinds', 'CodeableConcept') \"kind\";\n";
        // Definitions and ConceptMaps are found in subfolders too.
        let dir = folder(&[
            ("FacilityToLocation.map", &map("FacilityToLocation", body)),
            ("models/Facility.json", FACILITY),
            ("terminology/kinds.json", KINDS),
            ("notes.txt", "not read"),
        ]);
        let package = load(&dir).unwrap();
        assert_eq!(package.map_count(), 1);
        let map = package.only_map().unwrap();
        assert_eq!(
            first_source_url(map).as_deref(),
            Some("https://example.org/StructureDefinition/Facility")
        );
        let record =
            json!({"resourceType": "Facility", "code": "f1", "name": "Bo clinic", "kind": "site"});
        let out = package.transform(map, &record).unwrap();
        assert_eq!(
            out.resources,
            vec![
                json!({"resourceType": "Location", "id": "f1", "name": "Bo clinic",
                "physicalType": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/location-physical-type",
                                             "code": "si", "display": "Site"}]}})
            ]
        );
        assert!(out.warnings.is_empty());
        let (errors, warnings) = package.check();
        assert!(
            errors.is_empty() && warnings.is_empty(),
            "{errors:?} {warnings:?}"
        );
    }

    #[test]
    fn check_finds_a_misspelt_element_on_either_side() {
        let body =
            "  src.cod as c -> loc.id = c \"id\";\n  src.name as n -> loc.nmae = n \"name\";\n";
        let dir = folder(&[("m.map", &map("M", body)), ("Facility.json", FACILITY)]);
        let (errors, _) = load(&dir).unwrap().check();
        assert_eq!(errors.len(), 2, "{errors:?}");
        // file:line:column, then the message.
        assert!(errors[0].contains("m.map:8:"), "{errors:?}");
        assert!(errors[0].contains("has no element `cod`"), "{errors:?}");
        assert!(errors[1].contains("m.map:9:"), "{errors:?}");
        assert!(
            errors[1].contains("`Location` has no element `nmae`"),
            "{errors:?}"
        );
    }

    #[test]
    fn a_syntax_error_stops_the_load_and_names_the_line() {
        let dir = folder(&[("bad.map", &map("Bad", "  src.code as c -> -> loc.id;\n"))]);
        let m = message(load(&dir));
        assert!(m.contains("bad.map:8:"), "{m}");
    }

    #[test]
    fn the_map_for_a_record_type_must_be_the_only_one() {
        let body = "  src.code as c -> loc.id = c \"id\";\n";
        let dir = folder(&[
            ("a.map", &map("A", body)),
            ("b.map", &map("B", body)),
            ("Facility.json", FACILITY),
        ]);
        let package = load(&dir).unwrap();
        let m = message(package.map_for_source("https://example.org/StructureDefinition/Facility"));
        assert!(
            m.contains("more than one map reads") && m.contains("A (") && m.contains("B ("),
            "{m}"
        );
        let m = message(package.map_for_source(ORG_UNIT_MODEL));
        assert!(m.contains("no map reads"), "{m}");
        let m = message(package.only_map());
        assert!(m.contains("choose one with --map"), "{m}");
        assert_eq!(package.map_named("B").unwrap().name.as_deref(), Some("B"));
        assert!(
            package
                .map_named("https://example.org/StructureMap/A")
                .is_ok()
        );
        let m = message(package.map_named("C"));
        assert!(m.contains("no map is named C"), "{m}");
        let m = message(MapPackage::load(&[dir.path().join("missing")], &[]));
        assert!(m.contains("is not a folder of maps"), "{m}");
    }

    #[test]
    fn the_example_icr_maps_check_clean_and_cover_every_dhis2_record() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/maps/icr-dhis2");
        let package = MapPackage::load(&[dir], &MODELS).unwrap();
        let (errors, warnings) = package.check();
        assert!(
            errors.is_empty() && warnings.is_empty(),
            "{errors:?} {warnings:?}"
        );
        for model in [ORG_UNIT_MODEL, INDICATOR_MODEL, VALUE_MODEL] {
            assert!(package.map_for_source(model).is_ok(), "{model}");
        }
    }
}
