//! Resolving canonicals to artefacts. A canonical resolves from the built-in
//! artefacts first, then from each source in the order given: a directory of
//! JSON (ViewDefinitions, SUSHI's logical-model form of them, and SQLView or
//! SQLQuery Libraries), a FHIR package `.tgz` (a path or https URL), or
//! `name#version` in the FHIR package cache. Each artefact has one warehouse
//! table, named from its `name` as view tables are.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

use super::{Library, SQLQUERY_PROFILE, SQLVIEW_PROFILE, builtin};
use crate::error::usage;
use crate::views::run::{View, parse_view, table_name};

pub const LOCATION_URL: &str = "https://healthcampaigns.org/fhir/analytics/ViewDefinition/Location";
pub const HIERARCHY_URL: &str =
    "https://healthcampaigns.org/fhir/analytics/Library/LocationHierarchy";
pub const HIERARCHY_TABLE: &str = "location_hierarchy";

/// Tables fhir-pipes writes itself, which no artefact from a source may
/// write: the Location transform's and the built-in hierarchy's.
const RESERVED_TABLES: [&str; 2] = ["locations", HIERARCHY_TABLE];

/// A ViewDefinition or a SQLView/SQLQuery Library.
// Unboxed: a catalog holds tens of artefacts, so the size does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Artifact {
    View(View),
    Library(Library),
}

impl Artifact {
    pub fn url(&self) -> &str {
        match self {
            Artifact::View(v) => v.json["url"].as_str().unwrap_or_default(),
            Artifact::Library(l) => &l.url,
        }
    }

    pub fn version(&self) -> Option<&str> {
        match self {
            Artifact::View(v) => v.json["version"].as_str(),
            Artifact::Library(l) => l.version.as_deref(),
        }
    }

    /// `url|version`, or the url alone.
    pub fn canonical(&self) -> String {
        match self.version() {
            Some(v) => format!("{}|{v}", self.url()),
            None => self.url().to_string(),
        }
    }

    fn name(&self) -> &str {
        match self {
            Artifact::View(v) => &v.name,
            Artifact::Library(l) => &l.name,
        }
    }

    /// The warehouse table the artefact stands for. The built-in
    /// LocationHierarchy's is `location_hierarchy`, which the locations step
    /// writes, whatever `strip_prefix` says.
    pub fn table(&self, strip_prefix: &[String]) -> String {
        if self.url() == HIERARCHY_URL {
            return HIERARCHY_TABLE.to_string();
        }
        table_name(self.name(), strip_prefix)
    }
}

/// The artefacts by canonical URL, each URL's versions in the order read.
#[derive(Debug, Default)]
pub struct Catalog {
    artifacts: Vec<Artifact>,
    /// URL → indexes into `artifacts`.
    by_url: HashMap<String, Vec<usize>>,
}

impl Catalog {
    /// The built-ins, then the artefacts in `sources`. Relative paths resolve
    /// against `base`. A URL and version read a second time is skipped, so
    /// the first source that has it wins. Every Library's dependencies are
    /// resolved here, so a missing one or a cycle stops the load.
    pub fn load(sources: &[String], base: &Path, strip_prefix: &[String]) -> Result<Catalog> {
        let mut catalog = Catalog::default();
        for a in builtin::artifacts() {
            catalog.insert(a);
        }
        let builtins = catalog.artifacts.len();
        for source in sources {
            let spec = resolve_spec(source, base);
            let mut found = Vec::new();
            crate::fhir::package::read_json_files(&spec, "libraries", |name, v| {
                let a = artifact(&v, strip_prefix).with_context(|| format!("reading {name}"));
                found.push((name.to_string(), a));
            })
            .map_err(usage)?;
            // A directory is read in the file system's order; file-name order
            // keeps `libraries` and `order` the same from run to run.
            found.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, a) in found {
                let Some(a) = a.map_err(usage)? else {
                    continue;
                };
                let built_in = [LOCATION_URL, HIERARCHY_URL].contains(&a.url())
                    || catalog.artifacts[..builtins]
                        .iter()
                        .any(|b| b.url() == a.url());
                if built_in {
                    return Err(usage(anyhow!(
                        "libraries {source}: {} is built in to fhir-pipes and cannot be \
                         redefined; remove it from the source",
                        a.url()
                    )));
                }
                if catalog.find(a.url(), a.version()).is_none() {
                    catalog.insert(a);
                }
            }
        }
        catalog
            .check_tables(builtins, strip_prefix)
            .map_err(usage)?;
        catalog.order()?;
        Ok(catalog)
    }

    fn insert(&mut self, a: Artifact) {
        self.by_url
            .entry(a.url().to_string())
            .or_default()
            .push(self.artifacts.len());
        self.artifacts.push(a);
    }

    fn find(&self, url: &str, version: Option<&str>) -> Option<usize> {
        self.by_url
            .get(url)?
            .iter()
            .copied()
            .find(|&i| self.artifacts[i].version() == version)
    }

    /// Each artefact writes its own table, which no other artefact writes:
    /// not another URL, and not another version of its own URL, which a
    /// Library pinned to one version would otherwise read. For a SQLView the
    /// table is the name of its PostgreSQL view. An artefact from a source
    /// (past the first `builtins`) cannot write a reserved table.
    fn check_tables(&self, builtins: usize, strip_prefix: &[String]) -> Result<()> {
        let mut seen: BTreeMap<String, &Artifact> = BTreeMap::new();
        for (i, a) in self.artifacts.iter().enumerate() {
            let table = a.table(strip_prefix);
            let sql_name = table.starts_with(|c: char| c.is_ascii_lowercase())
                && table
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if !sql_name {
                bail!(
                    "{} would write table {table:?}, which is not a SQL name; rename it or \
                     change strip_prefix",
                    a.url()
                );
            }
            if i >= builtins && RESERVED_TABLES.contains(&table.as_str()) {
                bail!(
                    "{} would write table {table}, which is reserved for fhir-pipes; rename it",
                    a.url()
                );
            }
            // A URL and version is read once (`load`), so any artefact
            // already seen for this table is a different one.
            if let Some(other) = seen.get(&table) {
                bail!(
                    "{} and {} both write table {table}; give each a name of its own or \
                     change strip_prefix",
                    other.canonical(),
                    a.canonical()
                );
            }
            seen.insert(table, a);
        }
        Ok(())
    }

    /// The artefact at `canonical`, at `version` when given. Without a
    /// version the canonical must have only one.
    pub fn get(&self, canonical: &str, version: Option<&str>) -> Result<&Artifact> {
        Ok(&self.artifacts[self.index(canonical, version)?])
    }

    fn index(&self, canonical: &str, version: Option<&str>) -> Result<usize> {
        let ids = self.by_url.get(canonical).ok_or_else(|| {
            anyhow!("{canonical} is not built in and is in none of the library sources")
        })?;
        let versions = || {
            ids.iter()
                .map(|&i| self.artifacts[i].version().unwrap_or("(none)"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        match version {
            Some(v) => ids
                .iter()
                .copied()
                .find(|&i| self.artifacts[i].version() == Some(v))
                .ok_or_else(|| anyhow!("{canonical} has no version {v}, only {}", versions())),
            None if ids.len() == 1 => Ok(ids[0]),
            None => bail!(
                "{canonical} has more than one version ({}); name one as {canonical}|<version>",
                versions()
            ),
        }
    }

    /// Every Library, in the order read.
    pub fn libraries(&self) -> Vec<&Library> {
        self.artifacts
            .iter()
            .filter_map(|a| match a {
                Artifact::Library(l) => Some(l),
                Artifact::View(_) => None,
            })
            .collect()
    }

    /// Every Library, each after the Libraries it depends on. Views are
    /// leaves.
    pub fn order(&self) -> Result<Vec<&Library>> {
        let mut state = vec![Visit::New; self.artifacts.len()];
        let mut out = Vec::new();
        let mut path = Vec::new();
        for i in 0..self.artifacts.len() {
            self.visit(i, &mut state, &mut path, &mut out)
                .map_err(usage)?;
        }
        Ok(out)
    }

    /// Depth-first: `path` is the chain of Libraries being visited, to name a
    /// cycle.
    fn visit<'a>(
        &'a self,
        i: usize,
        state: &mut [Visit],
        path: &mut Vec<usize>,
        out: &mut Vec<&'a Library>,
    ) -> Result<()> {
        match state[i] {
            Visit::Done => return Ok(()),
            Visit::Open => {
                let start = path.iter().position(|&p| p == i).unwrap_or_default();
                let names: Vec<&str> = path[start..]
                    .iter()
                    .chain([&i])
                    .map(|&p| self.artifacts[p].name())
                    .collect();
                bail!(
                    "Libraries depend on each other in a cycle: {}",
                    names.join(" -> ")
                );
            }
            Visit::New => {}
        }
        let Artifact::Library(lib) = &self.artifacts[i] else {
            state[i] = Visit::Done;
            return Ok(());
        };
        state[i] = Visit::Open;
        path.push(i);
        for dep in &lib.depends_on {
            let resource = match &dep.version {
                Some(v) => format!("{}|{v}", dep.canonical),
                None => dep.canonical.clone(),
            };
            let what = || format!("Library {} depends-on {} ({resource})", lib.name, dep.label);
            // Only the built-in hierarchy reads the Location view.
            if dep.canonical == LOCATION_URL && lib.url != HIERARCHY_URL {
                bail!("{}: {}", what(), location_dependency());
            }
            let j = self
                .index(&dep.canonical, dep.version.as_deref())
                .with_context(what)?;
            self.visit(j, state, path, out)?;
        }
        path.pop();
        state[i] = Visit::Done;
        out.push(lib);
        Ok(())
    }
}

/// Why a Library cannot depend on the built-in Location view.
pub fn location_dependency() -> String {
    format!(
        "the built-in Location view writes no table; depend on LocationHierarchy \
         ({HIERARCHY_URL}) instead"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visit {
    New,
    Open,
    Done,
}

/// A source as `read_json_files` takes it: an https URL or `name#version` as
/// given, any other path against `base`.
fn resolve_spec(source: &str, base: &Path) -> String {
    let package = source.contains('#') && !source.contains('/');
    if source.starts_with("https://") || source.starts_with("http://") || package {
        return source.to_string();
    }
    base.join(source).to_string_lossy().into_owned()
}

/// The artefact `v` is, if any: a ViewDefinition with a url (plain, SUSHI's
/// logical-model form, or either inside a Binary), or a Library with the
/// SQLView or SQLQuery profile. Other resources are `None`.
fn artifact(v: &Value, strip_prefix: &[String]) -> Result<Option<Artifact>> {
    let kind = v["resourceType"].as_str().unwrap_or_default();
    if kind == "Library" {
        let profiles = v["meta"]["profile"].as_array().map(Vec::as_slice);
        let sql = profiles.is_some_and(|ps| {
            ps.iter()
                .any(|p| p == SQLVIEW_PROFILE || p == SQLQUERY_PROFILE)
        });
        return if sql {
            Ok(Some(Artifact::Library(Library::from_json(v)?)))
        } else {
            Ok(None)
        };
    }
    let text = if kind == "Binary" {
        // The IG publisher can carry a logical-model instance as a Binary
        // with the JSON in `data`.
        let Some(data) = v["data"].as_str() else {
            return Ok(None);
        };
        match STANDARD
            .decode(data)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
        {
            Some(t) => t,
            None => return Ok(None),
        }
    } else if kind == "ViewDefinition" || kind.ends_with("/ViewDefinition") {
        v.to_string()
    } else {
        return Ok(None);
    };
    let Some((json, definition)) = parse_view(&text)? else {
        return Ok(None);
    };
    let name = definition
        .name
        .clone()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow!("a ViewDefinition has no name"))?;
    // No Library can name a view without a url, so it is not an artefact; a
    // project's views directory (a source in a refresh) can have such views.
    if json["url"].as_str().is_none_or(str::is_empty) {
        return Ok(None);
    }
    Ok(Some(Artifact::View(View {
        table: table_name(&name, strip_prefix),
        resource: definition.resource.clone(),
        name,
        json,
        definition,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{SQLQUERY_PROFILE, SQLVIEW_PROFILE};
    use serde_json::{Value, json};
    use std::fs;

    const BASE: &str = "https://example.org/Library/";

    /// Library JSON: `deps` are (label, resource) pairs.
    fn lib(name: &str, version: &str, view: bool, deps: &[(&str, &str)]) -> Value {
        let profile = if view {
            SQLVIEW_PROFILE
        } else {
            SQLQUERY_PROFILE
        };
        let related: Vec<Value> = deps
            .iter()
            .map(|(l, r)| json!({"type": "depends-on", "label": l, "resource": r}))
            .collect();
        json!({
            "resourceType": "Library",
            "meta": {"profile": [profile]},
            "url": format!("{BASE}{name}"),
            "name": name,
            "version": version,
            "status": "draft",
            "relatedArtifact": related,
            "content": [],
        })
    }

    fn view(name: &str, url: &str) -> Value {
        json!({
            "resourceType": "ViewDefinition", "url": url, "name": name,
            "resource": "Patient", "status": "active",
            "select": [{"column": [{"name": "id", "path": "id"}]}],
        })
    }

    /// A temp dir holding each JSON as `<i>.json`.
    fn dir(files: &[Value]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (i, v) in files.iter().enumerate() {
            fs::write(d.path().join(format!("{i}.json")), v.to_string()).unwrap();
        }
        d
    }

    fn spec(d: &tempfile::TempDir) -> String {
        d.path().to_str().unwrap().to_string()
    }

    fn load(dirs: &[&tempfile::TempDir], strip: &[&str]) -> Result<Catalog> {
        let sources: Vec<String> = dirs.iter().map(|d| spec(d)).collect();
        let strip: Vec<String> = strip.iter().map(|s| s.to_string()).collect();
        Catalog::load(&sources, Path::new("/"), &strip)
    }

    fn names(libs: &[&Library]) -> Vec<String> {
        libs.iter().map(|l| l.name.clone()).collect()
    }

    #[test]
    fn a_canonical_resolves_from_the_first_source_that_has_it() {
        // Two versions of one canonical need names of their own: each
        // writes its own table.
        let a = dir(&[lib("Foo", "1", false, &[])]);
        let mut foo2 = lib("FooTwo", "2", false, &[]);
        foo2["url"] = json!(format!("{BASE}Foo"));
        let b = dir(&[foo2, lib("Foo", "1", false, &[])]);
        let cat = load(&[&a, &b], &[]).unwrap();
        let url = format!("{BASE}Foo");
        assert_eq!(cat.get(&url, Some("2")).unwrap().version(), Some("2"));
        assert_eq!(cat.get(&url, Some("1")).unwrap().version(), Some("1"));
        let err = cat.get(&url, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("more than one version"),
            "{err:#}"
        );
        assert!(cat.get(&format!("{BASE}Nope"), None).is_err());
    }

    #[test]
    fn a_view_without_a_url_is_skipped() {
        // A project's views directory is a source too; its views need no url,
        // but no Library can name one without.
        let mut no_url = view("IcrNoUrl", "");
        no_url.as_object_mut().unwrap().remove("url");
        let a = dir(&[no_url, view("IcrBlank", ""), lib("Foo", "1", false, &[])]);
        let cat = load(&[&a], &["icr_"]).unwrap();
        assert!(cat.get(&format!("{BASE}Foo"), None).is_ok());
        assert_eq!(
            cat.artifacts
                .iter()
                .filter(|a| matches!(a, Artifact::View(_)))
                .count(),
            1,
            "only the built-in Location view"
        );
    }

    #[test]
    fn a_source_cannot_define_a_built_in() {
        let mut v = lib("MyHierarchy", "1", true, &[]);
        v["url"] = json!(HIERARCHY_URL);
        let a = dir(&[v]);
        let err = load(&[&a], &[]).unwrap_err();
        assert!(format!("{err:#}").contains("built in"), "{err:#}");
    }

    #[test]
    fn the_built_ins_load_with_no_sources() {
        // LocationHierarchy writes the reserved table and depends on the
        // Location view, both of which only a built-in may do.
        let cat = Catalog::load(&[], Path::new("/"), &[]).unwrap();
        let h = cat.get(HIERARCHY_URL, None).unwrap();
        assert!(matches!(h, Artifact::Library(_)));
        assert_eq!(h.table(&[]), HIERARCHY_TABLE);
        assert!(matches!(
            cat.get(LOCATION_URL, None).unwrap(),
            Artifact::View(_)
        ));
        assert_eq!(names(&cat.order().unwrap()), ["LocationHierarchy"]);
    }

    #[test]
    fn two_artifacts_with_one_table_are_refused() {
        let a = dir(&[
            lib("IcrFoo", "1", false, &[]),
            view("IcrFoo", "https://example.org/ViewDefinition/IcrFoo"),
        ]);
        let err = load(&[&a], &["icr_"]).unwrap_err();
        assert!(
            format!("{err:#}").contains("both write table foo"),
            "{err:#}"
        );
    }

    #[test]
    fn two_versions_with_one_table_are_refused() {
        // A consumer pinned to Foo|1 would read version 2's result, or, for
        // SQLViews, the PostgreSQL view of version 2.
        for view in [false, true] {
            let a = dir(&[lib("Foo", "1", view, &[])]);
            let b = dir(&[lib("Foo", "2", view, &[])]);
            let err = format!("{:#}", load(&[&a, &b], &[]).unwrap_err());
            assert!(
                err.contains(&format!("{BASE}Foo|1"))
                    && err.contains(&format!("{BASE}Foo|2"))
                    && err.contains("both write table foo"),
                "{err}"
            );
        }
        // Views too: each version's table would be the same.
        let v = "https://example.org/ViewDefinition/V";
        let mut v2 = view("V", v);
        v2["version"] = json!("2");
        let mut v1 = view("V", v);
        v1["version"] = json!("1");
        let err = format!("{:#}", load(&[&dir(&[v1, v2])], &[]).unwrap_err());
        assert!(
            err.contains(&format!("{v}|1")) && err.contains(&format!("{v}|2")),
            "{err}"
        );
    }

    #[test]
    fn the_hierarchy_table_is_kept_whatever_the_strip_prefix() {
        // With location_ stripped, LocationHierarchy is still
        // location_hierarchy, so a Library named Hierarchy has a table of its
        // own.
        let a = dir(&[lib("Hierarchy", "1", false, &[])]);
        let strip = ["location_".to_string()];
        let cat = load(&[&a], &["location_"]).unwrap();
        assert_eq!(
            cat.get(HIERARCHY_URL, None).unwrap().table(&strip),
            HIERARCHY_TABLE
        );
        assert_eq!(
            cat.get(&format!("{BASE}Hierarchy"), None)
                .unwrap()
                .table(&strip),
            "hierarchy"
        );
    }

    #[test]
    fn the_reserved_tables_are_refused() {
        for name in ["Locations", "LocationHierarchy"] {
            let a = dir(&[lib(name, "1", true, &[])]);
            let err = load(&[&a], &[]).unwrap_err();
            assert!(format!("{err:#}").contains("reserved"), "{name}: {err:#}");
        }
    }

    #[test]
    fn order_puts_dependencies_first_and_finds_cycles() {
        let v = "https://example.org/ViewDefinition/Patients";
        let a = dir(&[
            lib("A", "1", false, &[("b", &format!("{BASE}B"))]),
            lib("B", "1", true, &[("p", v)]),
            view("Patients", v),
        ]);
        let cat = load(&[&a], &[]).unwrap();
        // The built-in LocationHierarchy is read first.
        assert_eq!(
            names(&cat.order().unwrap()),
            ["LocationHierarchy", "B", "A"]
        );
        assert_eq!(cat.libraries().len(), 3);

        let a = dir(&[
            lib("A", "1", true, &[("b", &format!("{BASE}B"))]),
            lib("B", "1", true, &[("a", &format!("{BASE}A|1"))]),
        ]);
        let err = load(&[&a], &[]).unwrap_err();
        assert!(format!("{err:#}").contains("cycle"), "{err:#}");
    }

    #[test]
    fn an_unresolved_dependency_names_the_library_and_label() {
        let missing = "https://example.org/Library/Missing";
        let a = dir(&[lib("A", "1", false, &[("gone", missing)])]);
        let err = format!("{:#}", load(&[&a], &[]).unwrap_err());
        for want in ["A", "gone", missing] {
            assert!(err.contains(want), "{want}: {err}");
        }
    }

    /// SUSHI writes a logical-model instance into Binary-*.json with the
    /// logical model's URL as its resourceType.
    fn sushi_view() -> Value {
        let mut v = view("IcrThing", "https://example.org/ViewDefinition/IcrThing");
        v["resourceType"] = json!("https://sql-on-fhir.org/ig/StructureDefinition/ViewDefinition");
        v
    }

    #[test]
    fn a_sushi_logical_model_view_is_read() {
        let a = dir(&[sushi_view()]);
        // The fixture IG's resources beside it: none of them is an artefact.
        let ig = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ig");
        let sources = [spec(&a), ig.to_str().unwrap().to_string()];
        let cat = Catalog::load(&sources, Path::new("/"), &["icr_".into()]).unwrap();
        let got = cat
            .get("https://example.org/ViewDefinition/IcrThing", None)
            .unwrap();
        assert!(matches!(got, Artifact::View(_)));
        assert_eq!(got.table(&["icr_".into()]), "thing");
        // Only the built-in LocationHierarchy.
        assert_eq!(names(&cat.libraries()), ["LocationHierarchy"]);
    }

    #[test]
    fn a_package_and_a_relative_directory_are_read() {
        let bytes = crate::fhir::package::tgz(&[
            (
                "package.json",
                r#"{"name":"example.libs","version":"1.0.0"}"#,
            ),
            ("Library-Foo.json", &lib("Foo", "1", false, &[]).to_string()),
            ("Binary-IcrThing.json", &sushi_view().to_string()),
        ]);
        let base = tempfile::tempdir().unwrap();
        fs::write(base.path().join("libs.tgz"), bytes).unwrap();
        fs::create_dir(base.path().join("more")).unwrap();
        fs::write(
            base.path().join("more/Bar.json"),
            lib("Bar", "1", false, &[]).to_string(),
        )
        .unwrap();
        let sources = ["libs.tgz".to_string(), "more".to_string()];
        let cat = Catalog::load(&sources, base.path(), &[]).unwrap();
        assert!(cat.get(&format!("{BASE}Foo"), None).is_ok());
        assert!(cat.get(&format!("{BASE}Bar"), None).is_ok());
        assert!(
            cat.get("https://example.org/ViewDefinition/IcrThing", None)
                .is_ok()
        );
    }
}
