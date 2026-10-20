//! The FHIR side of a refresh: the Location registry and the NDJSON downloads
//! the views read. These were `kiln run` and `kiln export` in a separate
//! process; they are now the same commands' code, called directly.

use std::path::Path;

use anyhow::Result;

use crate::cli::{
    DEFAULT_CONCURRENCY, DEFAULT_MAX_CONSECUTIVE_FAILURES, DEFAULT_PARTITION_BY, DEFAULT_RETRIES,
    DEFAULT_ROW_GROUP_SIZE, DEFAULT_TIMEOUT_SECS, ExportArgs, ExtractArgs, RunArgs, TransformArgs,
};
use crate::transform::SNAPSHOT_FILE;

use super::config::{Fetch, Settings};

/// `fhir-pipes run`: incremental extract into raw/<server>/locations, transform
/// into `out`/locations (the run's staging directory; it becomes
/// parquet/locations when the run is published). `full` re-extracts every
/// Location (the incremental extract cannot see deletions).
/// A snapshot built from files (files.rs) is replaced by a full extract.
/// With `reuse_raw` and a snapshot there, the snapshot is transformed without
/// an extract. Returns whether the server was contacted.
pub fn locations(s: &Settings, out: &Path) -> Result<bool> {
    let snapshot = s.raw_dir().join("locations");
    if s.reuse_raw && snapshot.join(SNAPSHOT_FILE).is_file() {
        println!(
            "== locations (--reuse-raw: {} → {}, no extract)",
            snapshot.display(),
            s.parquet_dir().join("locations").display()
        );
        crate::transform::run_transform(
            &TransformArgs {
                snapshot,
                out: out.to_path_buf(),
                country: None,
                row_group_size: DEFAULT_ROW_GROUP_SIZE,
                partition_by: DEFAULT_PARTITION_BY.to_string(),
                keep_without_geometry: false,
            },
            s.profile.as_deref(),
        )?;
        return Ok(false);
    }
    let from_files = crate::snapshot::State::read(&snapshot.join(crate::snapshot::STATE_FILE))?
        .is_some_and(|st| st.server == super::files::FILES);
    if from_files && !s.full {
        println!("   the snapshot was built from files: extracting every Location again");
    }
    println!(
        "== locations ({} → {} → {})",
        crate::redact::url(&s.server),
        snapshot.display(),
        s.parquet_dir().join("locations").display()
    );
    crate::run::run(
        &RunArgs {
            extract: ExtractArgs {
                server: s.server.clone(),
                auth: s.auth.clone(),
                server_kind: s.server_kind,
                snapshot,
                full: s.full || from_files,
                since: None,
                concurrency: DEFAULT_CONCURRENCY,
                retries: DEFAULT_RETRIES,
                max_consecutive_failures: DEFAULT_MAX_CONSECUTIVE_FAILURES,
                no_cache: false,
                refresh: false,
                cache_dir: None,
                timeout: DEFAULT_TIMEOUT_SECS,
            },
            out: out.to_path_buf(),
            country: None,
            row_group_size: DEFAULT_ROW_GROUP_SIZE,
            partition_by: DEFAULT_PARTITION_BY.to_string(),
            keep_without_geometry: false,
        },
        s.profile.as_deref(),
    )?;
    Ok(true)
}

/// `fhir-pipes export --incremental`: each type to raw/<server>/<Type>.ndjson,
/// downloading only what changed since the last refresh unless `full`, with
/// `server.concurrency` searches at a time. With `fetch = "bulk"`, through a
/// Bulk Data `$export` (`export --bulk`); with `fetch = "hapi-db"`, from the
/// HAPI server's database (`export --hapi-db`).
pub fn export(s: &Settings, types: &[&str]) -> Result<()> {
    if types.is_empty() {
        return Ok(());
    }
    let bulk = s.fetch == Fetch::Bulk;
    let hapi_db = s.fetch == Fetch::HapiDb;
    println!(
        "   export {}{} → {}",
        types.join(", "),
        match s.fetch {
            Fetch::Search => "",
            Fetch::Bulk => " ($export)",
            Fetch::HapiDb => " (HAPI database)",
        },
        s.raw_dir().display()
    );
    crate::export::run_export(&ExportArgs {
        server: s.server.clone(),
        auth: s.auth.clone(),
        server_kind: s.server_kind,
        types: types.iter().map(|t| t.to_string()).collect(),
        out: s.raw_dir(),
        incremental: !s.full,
        bulk,
        bulk_timeout: s.bulk_timeout,
        hapi_db,
        concurrency: s.concurrency,
        report: None,
        retries: DEFAULT_RETRIES,
        timeout: DEFAULT_TIMEOUT_SECS,
    })?;
    Ok(())
}
