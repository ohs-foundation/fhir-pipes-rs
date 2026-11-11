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
    /// `built_in` (JSON text), which a folder may not redefine.
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
                                return Err(usage(format!(
                                    "{}: {url} is built into fhir-pipes and cannot be redefined",
                                    path.display()
                                )));
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
