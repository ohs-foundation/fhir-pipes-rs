//! The FHIR side of a refresh: the Location registry and the NDJSON downloads
//! the views read. These were `kiln run` and `kiln export` in a separate
//! process; they are now the same commands' code, called directly.

use anyhow::Result;

use crate::cli::{
    DEFAULT_CONCURRENCY, DEFAULT_MAX_CONSECUTIVE_FAILURES, DEFAULT_PARTITION_BY, DEFAULT_RETRIES,
    DEFAULT_ROW_GROUP_SIZE, DEFAULT_TIMEOUT_SECS, ExportArgs, ExtractArgs, RunArgs,
};

use super::config::Settings;

/// `fhir-pipes run`: incremental extract into raw/<server>/locations, transform
/// into parquet/locations. `full` re-extracts every Location (the incremental
/// extract cannot see deletions).
pub fn locations(s: &Settings) -> Result<()> {
    let snapshot = s.raw_dir().join("locations");
    println!(
        "== locations ({} → {} → {})",
        s.server,
        snapshot.display(),
        s.parquet_dir().join("locations").display()
    );
    crate::run::run(&RunArgs {
        extract: ExtractArgs {
            server: s.server.clone(),
            auth: s.auth.clone(),
            snapshot,
            full: s.full,
            since: None,
            concurrency: DEFAULT_CONCURRENCY,
            retries: DEFAULT_RETRIES,
            max_consecutive_failures: DEFAULT_MAX_CONSECUTIVE_FAILURES,
            no_cache: false,
            refresh: false,
            cache_dir: None,
            timeout: DEFAULT_TIMEOUT_SECS,
        },
        out: s.parquet_dir(),
        country: None,
        row_group_size: DEFAULT_ROW_GROUP_SIZE,
        partition_by: DEFAULT_PARTITION_BY.to_string(),
        keep_without_geometry: false,
    })?;
    Ok(())
}

/// `fhir-pipes export --incremental`: each type to raw/<server>/<Type>.ndjson,
/// downloading only what changed since the last refresh unless `full`.
pub fn export(s: &Settings, types: &[&str]) -> Result<()> {
    if types.is_empty() {
        return Ok(());
    }
    println!("   export {} → {}", types.join(", "), s.raw_dir().display());
    crate::export::run_export(&ExportArgs {
        server: s.server.clone(),
        auth: s.auth.clone(),
        types: types.iter().map(|t| t.to_string()).collect(),
        out: s.raw_dir(),
        incremental: !s.full,
        report: None,
        retries: DEFAULT_RETRIES,
        timeout: DEFAULT_TIMEOUT_SECS,
    })?;
    Ok(())
}
