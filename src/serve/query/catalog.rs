//! Which SQLQuery Libraries `$sql-run` may run: the refresh's catalog,
//! filtered by `[serve.query] approve`.

use anyhow::{Result, bail};

use super::config::Approve;
use crate::library::resolve::{Artifact, Catalog};
use crate::library::{Kind, Library, split_canonical};
use crate::warehouse::config::Settings;

/// What a requested canonical is.
pub enum Lookup<'a> {
    /// An approved SQLQuery.
    Run(&'a Library),
    /// In the catalog, but a SQLView, a ViewDefinition, or a SQLQuery the
    /// rule does not approve.
    NotApproved,
    NotFound,
}

/// The catalog and the rule that approves its SQLQueries.
pub struct Queries {
    pub catalog: Catalog,
    approve: Approve,
    /// For `Listed`: the `url|version` of each listed Library.
    listed: Vec<String>,
}

impl Queries {
    /// The catalog of `s`, built as the refresh's libraries step builds it.
    /// With `Listed`, each entry must be a SQLQuery in it.
    pub fn load(s: &Settings, approve: &Approve) -> Result<Queries> {
        let catalog = crate::warehouse::libraries_step::catalog(s)?;
        let mut listed = Vec::new();
        if let Approve::Listed(entries) = approve {
            for entry in entries {
                let (url, version) = split_canonical(entry);
                match catalog.get(&url, version.as_deref()) {
                    Ok(Artifact::Library(l)) if l.kind == Kind::SqlQuery => {
                        listed.push(canonical(l));
                    }
                    Ok(_) => bail!("[serve.query] queries: {entry} is not a SQLQuery Library"),
                    Err(e) => bail!("[serve.query] queries: {entry}: {e:#}"),
                }
            }
        }
        Ok(Queries {
            catalog,
            approve: approve.clone(),
            listed,
        })
    }

    /// `canonical` (`url` or `url|version`) as a subject of `$sql-run`.
    pub fn lookup(&self, canonical: &str) -> Lookup<'_> {
        let (url, version) = split_canonical(canonical);
        match self.catalog.get(&url, version.as_deref()) {
            Err(_) => Lookup::NotFound,
            Ok(Artifact::Library(l)) if self.approves(l) => Lookup::Run(l),
            Ok(_) => Lookup::NotApproved,
        }
    }

    /// The SQLQueries that may be run, in the catalog's order.
    pub fn approved(&self) -> Vec<&Library> {
        self.catalog
            .libraries()
            .into_iter()
            .filter(|l| self.approves(l))
            .collect()
    }

    fn approves(&self, lib: &Library) -> bool {
        lib.kind == Kind::SqlQuery
            && match &self.approve {
                Approve::Active => lib.status.as_deref() == Some("active"),
                Approve::Listed(_) => self.listed.contains(&canonical(lib)),
                Approve::All => true,
            }
    }
}

/// `url|version`, or the url alone.
fn canonical(lib: &Library) -> String {
    match &lib.version {
        Some(v) => format!("{}|{v}", lib.url),
        None => lib.url.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::query::testing::{Warehouse, have_duckdb, library};

    const Q: &str = "https://example.org/Library/Q";

    fn ran(l: Lookup<'_>) -> Option<String> {
        match l {
            Lookup::Run(lib) => Some(lib.name.clone()),
            _ => None,
        }
    }

    #[test]
    fn lookup_follows_the_rule() {
        if !have_duckdb("lookup_follows_the_rule") {
            return;
        }
        let w = Warehouse::new();
        let load = |a: Approve| Queries::load(&w.settings(), &a).unwrap();
        let active = load(Approve::Active);
        assert!(matches!(active.lookup(Q), Lookup::NotApproved));
        assert!(active.approved().is_empty());
        let listed = load(Approve::Listed(vec![Q.into()]));
        assert_eq!(ran(listed.lookup(Q)).as_deref(), Some("Q"));
        assert!(matches!(
            listed.lookup("https://example.org/Library/Bad"),
            Lookup::NotApproved
        ));
        let all = load(Approve::All);
        assert_eq!(ran(all.lookup(Q)).as_deref(), Some("Q"));
        for view in [
            "https://example.org/Library/TaskBig",
            "https://example.org/ViewDefinition/Task",
        ] {
            assert!(matches!(all.lookup(view), Lookup::NotApproved), "{view}");
        }
        assert!(matches!(
            all.lookup("https://example.org/Library/Nope"),
            Lookup::NotFound
        ));

        w.set_status("q", "active");
        let active = load(Approve::Active);
        assert_eq!(ran(active.lookup(Q)).as_deref(), Some("Q"));
        let names: Vec<&str> = active.approved().iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["Q"]);
    }

    #[test]
    fn a_listed_entry_that_is_not_a_sqlquery_is_an_error() {
        if !have_duckdb("a_listed_entry_that_is_not_a_sqlquery_is_an_error") {
            return;
        }
        let w = Warehouse::new();
        for entry in [
            "https://example.org/Library/TaskBig",
            "https://example.org/Library/Nope",
        ] {
            let err = Queries::load(&w.settings(), &Approve::Listed(vec![entry.into()]))
                .err()
                .unwrap()
                .to_string();
            assert!(err.contains(entry), "{err}");
        }
    }

    #[test]
    fn a_versioned_canonical_resolves() {
        if !have_duckdb("a_versioned_canonical_resolves") {
            return;
        }
        let w = Warehouse::new();
        let url = "https://example.org/Library/V";
        for (file, version) in [("v1", "1"), ("v2", "2")] {
            let mut v = library(
                "V",
                false,
                &[("task", "https://example.org/ViewDefinition/Task")],
                "SELECT id FROM task",
            );
            v["version"] = version.into();
            v["name"] = format!("V{version}").into();
            w.write(file, &v);
        }
        let q = Queries::load(&w.settings(), &Approve::All).unwrap();
        assert_eq!(ran(q.lookup(&format!("{url}|2"))).as_deref(), Some("V2"));
        assert!(matches!(q.lookup(&format!("{url}|3")), Lookup::NotFound));
        // Two versions: the url alone names neither.
        assert!(matches!(q.lookup(url), Lookup::NotFound));
        let listed =
            Queries::load(&w.settings(), &Approve::Listed(vec![format!("{url}|1")])).unwrap();
        assert_eq!(
            ran(listed.lookup(&format!("{url}|1"))).as_deref(),
            Some("V1")
        );
        assert!(matches!(
            listed.lookup(&format!("{url}|2")),
            Lookup::NotApproved
        ));
    }
}
