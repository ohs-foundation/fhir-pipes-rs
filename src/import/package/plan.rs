//! A package loaded from its folder, and the plan of a run: each step with
//! the map, model, Library and views it names, resolved and checked.
//!
//! A view whose `resource` is one of the package's record types is a
//! record view: it reads the record file of that type, as the maps read it,
//! not staged resources. That goes beyond SQL on FHIR, so the manifest has
//! to allow it (`[views] records = "allow"`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use fml_model::StructureMap;

use super::manifest::{self, Check, Manifest, RecordRules, Source, Step};
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
    /// The views in `views/`, in file-name order.
    pub views: Vec<View>,
}

/// The variable a record view's paths read the record's line in its file
/// from, counted from 1: `%recordLine`.
pub const RECORD_LINE: &str = "recordLine";

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
        let mut maps = MapPackage::load(&[models_dir, maps_dir], &[])?;
        if let Some(n) = manifest.as_ref().and_then(|m| m.max_values) {
            maps.set_max_values(n);
        }
        maps.set_values_hint(&format!(
            "a record this large needs a higher limit: set max_values under [maps] in {}",
            manifest::FILE
        ));
        let views_dir = dir.join("views");
        let mut sources = Vec::new();
        let mut views = Vec::new();
        if views_dir.is_dir() {
            // Checks each view's structure, as the refresh does.
            views = crate::views::run::discover(&views_dir, &[]).map_err(usage)?;
            // Relative to the package folder, `base` below.
            sources.push("views".to_string());
        }
        let allowed = manifest.as_ref().is_some_and(|m| m.record_views);
        for v in &views {
            if models.get(&v.resource).is_some() {
                check_record_view(v, allowed)?;
            } else if let Some(p) = view_paths(v).into_iter().find(|p| uses_record_line(p)) {
                return Err(usage(anyhow!(
                    "view {} reads {}, and its path `{p}` uses %{RECORD_LINE}, which only a \
                     view over a record type has",
                    v.name,
                    v.resource
                )));
            }
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
            views,
        })
    }

    /// Whether `view` reads a record file rather than staged resources.
    pub fn is_record_view(&self, view: &View) -> bool {
        self.models.get(&view.resource).is_some()
    }

    /// The record views, in file-name order.
    pub fn record_views(&self) -> impl Iterator<Item = &View> {
        self.views.iter().filter(|v| self.is_record_view(v))
    }

    /// For each record view's model, the pairs of elements the view engine
    /// can read one for the other (`Models::misread_pairs`), as messages.
    pub fn record_view_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        for v in self.record_views() {
            for (parent, a, b) in self.models.misread_pairs(&v.resource) {
                out.push(format!(
                    "view {} ({}): {parent} has elements `{a}` and `{b}`: in a record with no \
                     `{a}`, the view engine reads `{b}` for a path that reads `{a}`, as if `{a}` \
                     were a choice element",
                    v.name, v.resource
                ));
            }
        }
        out
    }

    /// The paths of the record views that name an element their model does
    /// not have, each as one message naming the view, the path and the
    /// model. Only plain chains of element names are followed (see
    /// `fhir_pipes_sof_lint::lint_selectors`); a path it cannot follow is
    /// not reported.
    pub fn record_view_problems(&self) -> Vec<String> {
        let elements = self.models.elements();
        let mut out = Vec::new();
        for v in self.record_views() {
            let model = self.models.get(&v.resource).expect("a record view's model");
            let columns: Vec<(&str, &str)> = v
                .definition
                .selects()
                .into_iter()
                .flat_map(|s| s.column.iter().flatten())
                .map(|c| (c.name.as_str(), c.path.as_str()))
                .collect();
            for f in fhir_pipes_sof_lint::lint_selectors(&v.definition, &elements) {
                if f.code != "FH01" {
                    continue;
                }
                let location = f.location.as_deref().unwrap_or_default();
                // A column by its name; `unionAll` branches repeat the names,
                // so the one whose path has the unknown name.
                let unknown = f.message.split('`').nth(1).unwrap_or_default();
                let named: Vec<&str> = columns
                    .iter()
                    .filter(|(n, _)| *n == location)
                    .map(|(_, p)| *p)
                    .collect();
                let path = named.iter().find(|p| p.contains(unknown)).or(named.first());
                let at = match path {
                    Some(path) => format!("column `{location}`, path `{path}`"),
                    None => format!("path `{location}`"),
                };
                let message = f
                    .message
                    .replacen("unknown FHIR element", "unknown element", 1);
                out.push(format!(
                    "view {} ({}, {}): {at}: {message}",
                    v.name, v.resource, model.url
                ));
            }
        }
        out
    }

    /// How the record files are read: the manifest's `[records]`.
    pub fn record_rules(&self) -> RecordRules {
        self.manifest
            .as_ref()
            .map_or(RecordRules::default(), |m| m.records)
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

/// A view over a record type is refused unless the manifest allows record
/// views, and when it does, may not call `getResourceKey()`: a record is
/// not a resource and has no key of its own.
fn check_record_view(v: &View, allowed: bool) -> Result<()> {
    if !allowed {
        return Err(usage(anyhow!(
            "view {} reads record type {}: a view over a record type reads the record \
             file directly, which goes beyond the SQL on FHIR specification. Set \
             `records = \"allow\"` under [views] in {} to allow it",
            v.name,
            v.resource,
            manifest::FILE
        )));
    }
    for p in view_paths(v) {
        if fhir_pipes_sof::calls_any(p, &["getResourceKey"]) == Some(true) {
            return Err(usage(anyhow!(
                "view {} reads record type {}, and its path `{p}` calls getResourceKey(): \
                 a record is not a resource and has no key of its own. Select the field \
                 that identifies the record by its path (such as `id`)",
                v.name,
                v.resource
            )));
        }
    }
    Ok(())
}

/// Every FHIRPath path of a view: `where`, columns, `forEach`,
/// `forEachOrNull` and `repeat`, nested selects and `unionAll` too.
fn view_paths(v: &View) -> Vec<&str> {
    let d = &v.definition;
    let mut paths: Vec<&str> = d.where_.iter().map(|w| w.path.as_str()).collect();
    for s in d.selects() {
        paths.extend(s.column.iter().flatten().map(|c| c.path.as_str()));
        paths.extend(s.for_each.as_deref());
        paths.extend(s.for_each_or_null.as_deref());
        paths.extend(s.repeat.iter().map(String::as_str));
    }
    paths
}

/// Whether `path` names `%recordLine` (and not a longer name that starts
/// with it).
fn uses_record_line(path: &str) -> bool {
    let name = format!("%{RECORD_LINE}");
    path.match_indices(&name).any(|(i, _)| {
        !path[i + name.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
    })
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
        /// The views to run first, which no earlier step ran: views over
        /// staged resources and record views.
        views: Vec<&'p View>,
        /// The record types the record views among `views` read.
        record_types: BTreeSet<String>,
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
                    let record_types = views
                        .iter()
                        .filter(|v| package.is_record_view(v))
                        .map(|v| v.resource.clone())
                        .collect();
                    PlanStep::Library {
                        library: lib,
                        views,
                        record_types,
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

    /// The record types the steps read from files: in map steps, and in the
    /// record views of library steps.
    pub fn record_types(&self) -> BTreeSet<&str> {
        let mut types = BTreeSet::new();
        for s in &self.steps {
            match s {
                PlanStep::Map {
                    input: Input::File(t),
                    ..
                } => {
                    types.insert(t.as_str());
                }
                PlanStep::Library { record_types, .. } => {
                    types.extend(record_types.iter().map(String::as_str));
                }
                PlanStep::Map { .. } => {}
            }
        }
        types
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
