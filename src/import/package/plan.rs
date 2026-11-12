//! A package loaded from its folder, and the plan of a run: each step with
//! the map, model, Library and views it names, resolved and checked.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use fml_model::StructureMap;

use super::manifest::{self, Check, Manifest, Source, Step};
use super::records::{Model, Models};
use crate::error::usage;
use crate::library::resolve::{Artifact, Catalog, HIERARCHY_URL, LOCATION_URL};
use crate::library::{Kind, Library};
use crate::mapping::{MapPackage, first_source_url};
use crate::views::run::View;

/// A package folder, loaded.
pub struct Package {
    pub dir: PathBuf,
    pub name: String,
    pub manifest: Option<Manifest>,
    pub maps: MapPackage,
    pub models: Models,
    pub catalog: Catalog,
}

impl Package {
    /// Reads `import.toml` (if there is one), `models/`, `maps/`, `views/`
    /// and `libraries/`. Anything wrong with them is a usage error.
    pub fn load(dir: &Path) -> Result<Self> {
        if !dir.is_dir() {
            return Err(usage(anyhow!("{} is not a package folder", dir.display())));
        }
        let manifest_path = dir.join(manifest::FILE);
        let manifest = if manifest_path.is_file() {
            let text = std::fs::read_to_string(&manifest_path)
                .with_context(|| format!("reading {}", manifest_path.display()))
                .map_err(usage)?;
            Some(manifest::parse(
                &text,
                &manifest_path.display().to_string(),
            )?)
        } else {
            None
        };
        let name = match &manifest {
            Some(m) => m.name.clone(),
            None => dir
                .canonicalize()
                .ok()
                .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| dir.display().to_string()),
        };
        let models_dir = dir.join("models");
        let maps_dir = dir.join("maps");
        for d in [&models_dir, &maps_dir] {
            if !d.is_dir() {
                return Err(usage(anyhow!(
                    "{} has no {}/ folder",
                    dir.display(),
                    d.file_name().unwrap_or_default().to_string_lossy()
                )));
            }
        }
        let models = Models::load(&models_dir)?;
        let maps = MapPackage::load(&[models_dir, maps_dir], &[])?;
        let views_dir = dir.join("views");
        let mut sources = Vec::new();
        if views_dir.is_dir() {
            // Checks each view's structure, as the refresh does.
            crate::views::run::discover(&views_dir, &[]).map_err(usage)?;
            // Relative to the package folder, `base` below.
            sources.push("views".to_string());
        }
        let libraries_dir = dir.join("libraries");
        let built = if libraries_dir.is_dir() {
            crate::library::source::build_all(&libraries_dir)?
        } else {
            Vec::new()
        };
        let catalog = Catalog::load_with(&sources, Some(("libraries/", built)), dir, &[])?;
        Ok(Self {
            dir: dir.to_path_buf(),
            name,
            manifest,
            maps,
            models,
            catalog,
        })
    }

    /// The SQLQuery Library with this name, URL or `url|version`.
    fn library(&self, name: &str) -> Result<&Library> {
        let all = self.catalog.libraries();
        let found: Vec<&Library> = all
            .iter()
            .copied()
            .filter(|l| l.url != HIERARCHY_URL)
            .filter(|l| {
                l.name == name
                    || l.url == name
                    || l.version
                        .as_ref()
                        .is_some_and(|v| format!("{}|{v}", l.url) == name)
            })
            .collect();
        match found.as_slice() {
            [one] if one.kind == Kind::SqlQuery => Ok(one),
            [one] => bail!(
                "{} is a SQLView; a step runs a SQLQuery Library, which reads it",
                one.name
            ),
            [] => {
                let names: Vec<&str> = all
                    .iter()
                    .filter(|l| l.kind == Kind::SqlQuery && l.url != HIERARCHY_URL)
                    .map(|l| l.name.as_str())
                    .collect();
                bail!(
                    "no Library is named {name}; the SQLQuery Libraries in libraries/ are: {}",
                    if names.is_empty() {
                        "none".to_string()
                    } else {
                        names.join(", ")
                    }
                )
            }
            _ => bail!("more than one Library is named {name}; name it by url|version"),
        }
    }
}

/// Where a map step's records come from.
pub enum Input<'p> {
    /// The record file of this type in `--in`.
    File(String),
    /// The rows of a Library's result.
    Rows(&'p Library),
}

pub enum PlanStep<'p> {
    Map {
        /// The map's name, for messages.
        name: String,
        map: &'p StructureMap,
        /// The model of the records the map reads.
        model: &'p Model,
        input: Input<'p>,
        stage: bool,
    },
    Library {
        library: &'p Library,
        /// The views to run first, which no earlier step ran.
        views: Vec<&'p View>,
        /// The SQLQuery Libraries to run, each after those it reads, ending
        /// with `library` unless an earlier step ran it.
        queries: Vec<&'p Library>,
    },
}

/// One data test, resolved: a query whose rows are the failures.
pub struct PlannedTest {
    pub label: String,
    pub check: PlannedCheck,
}

pub enum PlannedCheck {
    Unique { table: String, column: String },
    NotNull { table: String, column: String },
    Sql(String),
}

impl PlannedCheck {
    pub fn describe(&self) -> String {
        match self {
            PlannedCheck::Unique { table, column } => format!("unique {column} in {table}"),
            PlannedCheck::NotNull { table, column } => format!("not_null {column} in {table}"),
            PlannedCheck::Sql(_) => "sql".to_string(),
        }
    }
}

pub struct Plan<'p> {
    pub steps: Vec<PlanStep<'p>>,
    pub tests: Vec<PlannedTest>,
    /// The data tests run after this step: the last library step.
    pub tests_after: Option<usize>,
}

impl<'p> Plan<'p> {
    /// The plan of the package's manifest steps, or with no manifest or a
    /// manifest with no steps, one step per record file in `files`.
    pub fn new(package: &'p Package, files: Option<&BTreeMap<String, PathBuf>>) -> Result<Self> {
        match &package.manifest {
            Some(m) if !m.steps.is_empty() => Self::from_manifest(package, m),
            _ => match files {
                Some(files) => Self::by_record_type(package, files),
                None => Ok(Plan {
                    steps: Vec::new(),
                    tests: Vec::new(),
                    tests_after: None,
                }),
            },
        }
    }

    fn from_manifest(package: &'p Package, manifest: &Manifest) -> Result<Self> {
        let mut steps = Vec::new();
        let mut ran = Ran::default();
        let mut tests_after = None;
        for (i, step) in manifest.steps.iter().enumerate() {
            let at = || format!("{}: step {}", manifest::FILE, i + 1);
            let planned = match step {
                Step::Map { map, from, stage } => {
                    let found = package
                        .maps
                        .map_named(map)
                        .with_context(at)
                        .map_err(usage)?;
                    let source = first_source_url(found)
                        .ok_or_else(|| anyhow!("map {map}: its first group reads no source"))
                        .with_context(at)
                        .map_err(usage)?;
                    let (model, input) = match from {
                        Source::Records(t) => {
                            let model = package.models.get(t).ok_or_else(|| {
                                usage(anyhow!(
                                    "{}: no logical model in models/ is record type {t}",
                                    at()
                                ))
                            })?;
                            if model.url != source {
                                return Err(usage(anyhow!(
                                    "{}: map {map} reads {source}, not {t} ({})",
                                    at(),
                                    model.url
                                )));
                            }
                            (model, Input::File(t.clone()))
                        }
                        Source::Library(l) => {
                            let lib = package.library(l).with_context(at).map_err(usage)?;
                            if !ran.queries.contains(&key(lib)) {
                                return Err(usage(anyhow!(
                                    "{}: reads Library {l}, which no earlier library step runs",
                                    at()
                                )));
                            }
                            let model = package.models.by_url(&source).ok_or_else(|| {
                                usage(anyhow!(
                                    "{}: map {map} reads {source}, which is not a logical \
                                     model in models/",
                                    at()
                                ))
                            })?;
                            (model, Input::Rows(lib))
                        }
                    };
                    PlanStep::Map {
                        name: map.clone(),
                        map: found,
                        model,
                        input,
                        stage: *stage,
                    }
                }
                Step::Library { library } => {
                    let lib = package.library(library).with_context(at).map_err(usage)?;
                    let mut views = Vec::new();
                    let mut queries = Vec::new();
                    ran.needs(&package.catalog, lib, &mut views, &mut queries)
                        .with_context(at)
                        .map_err(usage)?;
                    if ran.queries.insert(key(lib)) {
                        queries.push(lib);
                    }
                    tests_after = Some(i);
                    PlanStep::Library {
                        library: lib,
                        views,
                        queries,
                    }
                }
            };
            steps.push(planned);
        }
        let mut tests = Vec::new();
        for (i, t) in manifest.tests.iter().enumerate() {
            let label = t.label(i);
            match &t.check {
                Check::Columns {
                    library,
                    unique,
                    not_null,
                } => {
                    let at = || format!("{}: test {}", manifest::FILE, i + 1);
                    let lib = package.library(library).with_context(at).map_err(usage)?;
                    if !ran.queries.contains(&key(lib)) {
                        return Err(usage(anyhow!(
                            "{}: checks Library {library}, which no library step runs",
                            at()
                        )));
                    }
                    let table = lib.table(&[]);
                    for column in unique {
                        tests.push(PlannedTest {
                            label: label.clone(),
                            check: PlannedCheck::Unique {
                                table: table.clone(),
                                column: column.clone(),
                            },
                        });
                    }
                    for column in not_null {
                        tests.push(PlannedTest {
                            label: label.clone(),
                            check: PlannedCheck::NotNull {
                                table: table.clone(),
                                column: column.clone(),
                            },
                        });
                    }
                }
                Check::Sql(sql) => tests.push(PlannedTest {
                    label,
                    check: PlannedCheck::Sql(sql.clone()),
                }),
            }
        }
        Ok(Plan {
            steps,
            tests,
            tests_after,
        })
    }

    /// With no manifest: each record file through the one map whose first
    /// group reads its type, the resources to the output.
    fn by_record_type(package: &'p Package, files: &BTreeMap<String, PathBuf>) -> Result<Self> {
        let mut steps = Vec::new();
        for (t, path) in files {
            let model = package.models.get(t).ok_or_else(|| {
                usage(anyhow!(
                    "{}: no logical model in models/ is record type {t}",
                    path.display()
                ))
            })?;
            let map = package
                .maps
                .map_for_source(&model.url)
                .with_context(|| format!("{}", path.display()))
                .map_err(usage)?;
            steps.push(PlanStep::Map {
                name: map.name.clone().unwrap_or_else(|| "unnamed".into()),
                map,
                model,
                input: Input::File(t.clone()),
                stage: false,
            });
        }
        Ok(Plan {
            steps,
            tests: Vec::new(),
            tests_after: None,
        })
    }

    /// The record types the steps read from files.
    pub fn record_types(&self) -> BTreeSet<&str> {
        self.steps
            .iter()
            .filter_map(|s| match s {
                PlanStep::Map {
                    input: Input::File(t),
                    ..
                } => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Whether any step runs a Library, which needs duckdb.
    pub fn runs_libraries(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s, PlanStep::Library { .. }))
    }
}

/// A Library's identity: url and version.
fn key(lib: &Library) -> (String, Option<String>) {
    (lib.url.clone(), lib.version.clone())
}

/// The views and SQLQuery Libraries the steps so far run.
#[derive(Default)]
struct Ran {
    views: BTreeSet<String>,
    queries: BTreeSet<(String, Option<String>)>,
}

impl Ran {
    /// Adds to `views` and `queries` what `lib` reads that has not run yet,
    /// each SQLQuery after what it reads. A SQLView is inlined, so only what
    /// it reads runs.
    fn needs<'p>(
        &mut self,
        catalog: &'p Catalog,
        lib: &'p Library,
        views: &mut Vec<&'p View>,
        queries: &mut Vec<&'p Library>,
    ) -> Result<()> {
        for dep in &lib.depends_on {
            let input = catalog
                .get(&dep.canonical, dep.version.as_deref())
                .with_context(|| format!("Library {} depends-on {}", lib.name, dep.label))?;
            match input {
                Artifact::View(v) => {
                    if v.json["url"] == LOCATION_URL {
                        bail!(
                            "Library {} reads the built-in Location view, which import does \
                             not run",
                            lib.name
                        );
                    }
                    if self.views.insert(v.table.clone()) {
                        views.push(v);
                    }
                }
                Artifact::Library(l) if l.url == HIERARCHY_URL => bail!(
                    "Library {} reads LocationHierarchy, a table the refresh writes; import \
                     has no such table",
                    lib.name
                ),
                Artifact::Library(l) if l.kind == Kind::SqlView => {
                    self.needs(catalog, l, views, queries)?
                }
                Artifact::Library(l) => {
                    if !self.queries.contains(&key(l)) {
                        self.needs(catalog, l, views, queries)?;
                        self.queries.insert(key(l));
                        queries.push(l);
                    }
                }
            }
        }
        Ok(())
    }
}
