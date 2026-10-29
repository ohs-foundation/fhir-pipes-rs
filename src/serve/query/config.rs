//! The `[serve.query]` section: the `$sql-run` API's settings.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryFile {
    approve: Option<String>,
    queries: Option<Vec<String>>,
    source: Option<String>,
    duckdb_setup: Option<String>,
    timeout: Option<u64>,
    max_rows: Option<usize>,
    concurrency: Option<usize>,
}

/// Which SQLQuery resources may be run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approve {
    /// Every query whose status is active.
    Active,
    /// Only the named queries.
    Listed(Vec<String>),
    /// Any query, whatever its status.
    All,
}

/// Where the warehouse files are read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Local,
    /// A remote prefix, without a trailing `/`.
    Remote(String),
}

// Read once the query API is wired in.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct QueryConfig {
    pub approve: Approve,
    pub source: Source,
    pub duckdb_setup: Option<PathBuf>,
    pub timeout: Duration,
    pub max_rows: usize,
    pub concurrency: usize,
}

/// The `[serve.query]` section. `base` is the config file's directory.
pub fn parse(section: toml::Table, base: &Path) -> Result<QueryConfig> {
    let file: QueryFile = toml::Value::Table(section).try_into()?;
    let approve = match (file.approve.as_deref().unwrap_or("active"), file.queries) {
        ("active", None) => Approve::Active,
        ("all", None) => Approve::All,
        ("listed", Some(q)) => Approve::Listed(q),
        ("listed", None) => bail!("[serve.query] approve = \"listed\" needs queries = [...]"),
        ("active" | "all", Some(_)) => {
            bail!("[serve.query] queries needs approve = \"listed\"")
        }
        (other, _) => {
            bail!("[serve.query] approve = \"{other}\": use \"active\", \"listed\" or \"all\"")
        }
    };
    let source = match file.source.as_deref() {
        None | Some("local") => Source::Local,
        Some(s) if s.contains("://") => Source::Remote(s.trim_end_matches('/').to_string()),
        Some(s) => bail!("[serve.query] source = \"{s}\": use \"local\" or a URL prefix"),
    };
    let timeout = file.timeout.unwrap_or(60);
    if timeout == 0 {
        bail!("[serve.query] timeout = 0: use at least 1 second");
    }
    let max_rows = file.max_rows.unwrap_or(100_000);
    if max_rows == 0 {
        bail!("[serve.query] max_rows = 0: use at least 1");
    }
    let concurrency = file.concurrency.unwrap_or(4);
    if concurrency == 0 {
        bail!("[serve.query] concurrency = 0: use at least 1");
    }
    Ok(QueryConfig {
        approve,
        source,
        duckdb_setup: file.duckdb_setup.map(|p| base.join(p)),
        timeout: Duration::from_secs(timeout),
        max_rows,
        concurrency,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> Result<QueryConfig> {
        parse(toml::from_str(text).unwrap(), Path::new("/etc/fp"))
    }

    #[test]
    fn the_defaults_are_the_specs() {
        let c = run("").unwrap();
        assert_eq!(c.approve, Approve::Active);
        assert_eq!(c.source, Source::Local);
        assert_eq!(c.duckdb_setup, None);
        assert_eq!(c.timeout, Duration::from_secs(60));
        assert_eq!(c.max_rows, 100_000);
        assert_eq!(c.concurrency, 4);
    }

    #[test]
    fn listed_needs_queries_and_queries_need_listed() {
        let err = run("approve = \"listed\"").unwrap_err().to_string();
        assert!(err.contains("queries"), "{err}");
        let err = run("approve = \"active\"\nqueries = [\"x\"]")
            .unwrap_err()
            .to_string();
        assert!(err.contains("approve = \"listed\""), "{err}");
        let c = run("approve = \"listed\"\nqueries = [\"x\"]").unwrap();
        assert_eq!(c.approve, Approve::Listed(vec!["x".into()]));
    }

    #[test]
    fn a_remote_source_and_its_setup() {
        let c = run("source = \"r2://icr/uga/\"\nduckdb_setup = \"q.sql\"").unwrap();
        assert_eq!(c.source, Source::Remote("r2://icr/uga".into()));
        assert_eq!(c.duckdb_setup, Some(Path::new("/etc/fp").join("q.sql")));
    }

    #[test]
    fn bad_values_are_refused() {
        for text in [
            "approve = \"some\"",
            "timeout = 0",
            "max_rows = 0",
            "concurrency = 0",
            "colour = 1",
        ] {
            assert!(run(text).is_err(), "{text}");
        }
    }
}
