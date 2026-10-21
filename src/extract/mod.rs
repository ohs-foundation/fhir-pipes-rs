//! `fhir-pipes extract`: page, fetch, merge. See the spec for the phase contracts.

pub mod boundary;
pub mod cache;
pub mod page;

use std::collections::HashMap;
use std::time::Duration;

use crate::cli::ExtractArgs;
use crate::error::{PipesError, Result};
use crate::extract::boundary::{FetchOptions, fetch_boundaries};
use crate::extract::cache::Cache;
use crate::extract::page::{CountCheck, Search, check_count, page_resources};
use crate::http::FhirClient;
use crate::report::{Report, write_report};
use crate::snapshot::instant::{capped_watermark, format_utc, parse_instant};
use crate::snapshot::merge::{MergeFiles, MergeOptions, merge, merge_file};
use crate::snapshot::{Snapshot, State, same_server};

/// Fetch Locations into the snapshot, incrementally unless `--full`.
///
/// Nothing before the merge touches `locations.ndjson` or `state.json`, so a
/// failure in the page or fetch phase leaves the previous snapshot intact.
///
/// `boundary_extensions` are the profile's: boundary attachments under them
/// are fetched and inlined like those under the HL7 extension.
pub fn run_extract(args: &ExtractArgs, boundary_extensions: &[String]) -> anyhow::Result<()> {
    if args.no_cache && (args.refresh || args.cache_dir.is_some()) {
        return Err(PipesError::Usage(
            "--no-cache cannot be combined with --refresh or --cache-dir".into(),
        )
        .into());
    }
    if args.concurrency == 0 {
        return Err(PipesError::Usage("--concurrency must be at least 1".into()).into());
    }
    if args.full && args.since.is_some() {
        return Err(PipesError::Usage("--since has no effect with --full; drop one".into()).into());
    }
    if let Some(s) = &args.since
        && parse_instant(s).is_none()
    {
        return Err(PipesError::Usage(format!(
            "--since {s}: not a FHIR instant (e.g. 2026-01-02T03:04:05Z)"
        ))
        .into());
    }
    let snap = Snapshot::new(&args.snapshot);
    std::fs::create_dir_all(&snap.dir)
        .map_err(|e| PipesError::Usage(format!("--snapshot {}: {e}", snap.dir.display())))?;
    // Fail fast, before any paging or fetching, if the snapshot directory
    // exists but isn't writable (e.g. read-only permissions): the same
    // mistake would otherwise surface much later as an opaque I/O error
    // from the merge or state write.
    let probe = snap.dir.join(".fhir-pipes-write-test");
    std::fs::write(&probe, b"")
        .and_then(|_| std::fs::remove_file(&probe))
        .map_err(|e| {
            PipesError::Usage(format!(
                "--snapshot {}: not writable: {e}",
                snap.dir.display()
            ))
        })?;

    // Mode: full, or incremental from --since or the stored watermark. The
    // snapshot belongs to one server: any incremental run against a
    // different one -- including one pinned with --since -- would silently
    // mix two registries, so it is a usage error. --full repoints.
    let state = State::read(&snap.state_path())?;
    if !args.full
        && let Some(st) = &state
        && !same_server(&st.server, &args.server)
    {
        return Err(PipesError::Usage(format!(
            "snapshot was extracted from {} but --server is {}; pass --full to repoint it",
            st.server, args.server
        ))
        .into());
    }
    let since: Option<String> = if args.full {
        None
    } else if let Some(s) = &args.since {
        Some(s.clone())
    } else {
        state.as_ref().and_then(|st| st.watermark.clone())
    };
    eprintln!(
        "{}",
        match &since {
            Some(s) => format!("incremental extract since {s}"),
            None => "full extract".to_string(),
        }
    );

    // Organizations have their own watermark. A snapshot from before they
    // were extracted has no file and no watermark, so they are fetched in
    // full while Locations stay incremental.
    let org_since: Option<String> = if args.full {
        None
    } else if let Some(s) = &args.since {
        Some(s.clone())
    } else if snap.organizations().exists() {
        state
            .as_ref()
            .and_then(|st| st.organization_watermark.clone())
    } else {
        None
    };
    let result = run_phases(
        args,
        &snap,
        since.as_deref(),
        org_since.as_deref(),
        boundary_extensions,
    );
    if result.is_err() {
        // The incoming files describe a run that never finished. The next
        // run truncates them before paging, so keeping them buys nothing and
        // they would only mislead anyone inspecting the snapshot directory.
        let _ = std::fs::remove_file(snap.incoming());
        let _ = std::fs::remove_file(snap.incoming_organizations());
    }
    Ok(result?)
}

/// The three phases. Split out so `run_extract` can clean up the incoming
/// file on any failure between creating it and the merge that consumes it.
fn run_phases(
    args: &ExtractArgs,
    snap: &Snapshot,
    since: Option<&str>,
    org_since: Option<&str>,
    boundary_extensions: &[String],
) -> Result<()> {
    let full = since.is_none();
    let client = FhirClient::new(
        args.auth.auth()?,
        args.retries,
        Duration::from_secs(args.timeout),
    )?
    .for_server(&args.server);
    let mut report = Report::default();

    let paged = page_resources(
        &client,
        &Search {
            boundary_extensions,
            ..Search::new(&args.server, args.server_kind, "Location")
        },
        since,
        &snap.incoming(),
        &mut report,
    )?;
    // Reported here rather than inside the pager so every phase summary is
    // printed by the orchestrator; the pager returns the moment it is done,
    // so this still lands before the (much longer) fetch phase starts.
    eprintln!(
        "paged {} resources over {} page(s)",
        paged.notes.len(),
        paged.pages
    );

    let mut urls: Vec<String> = paged
        .notes
        .iter()
        .filter_map(|n| n.boundary_url.clone())
        .collect();
    urls.sort();
    urls.dedup();
    let cache = if args.no_cache {
        None
    } else {
        Some(Cache::new(
            &args.cache_dir.clone().unwrap_or_else(|| snap.boundaries()),
        ))
    };
    let opts = FetchOptions {
        concurrency: args.concurrency,
        cache: cache.clone(),
        refresh: args.refresh,
        max_consecutive_failures: args.max_consecutive_failures,
        // With a trailing slash, so `Binary/1` joins under the base rather
        // than replacing its last segment.
        base: url::Url::parse(&format!("{}/", args.server.trim_end_matches('/'))).ok(),
    };
    let summary = fetch_boundaries(&client, &urls, &opts, &mut report)?;

    // The merge reads boundary bytes back by URL: from memory with
    // --no-cache (or after a cache write failed), from the cache otherwise.
    let in_memory: HashMap<String, Vec<u8>> = summary.in_memory;
    let lookup = |url: &str| -> Option<Vec<u8>> {
        if let Some(b) = in_memory.get(url) {
            return Some(b.clone());
        }
        cache.as_ref().and_then(|c| c.read(url).ok().flatten())
    };
    let stats = merge(
        snap,
        &paged.notes,
        full,
        &lookup,
        &summary.failures,
        &mut report,
        boundary_extensions,
    )?;

    // --refresh skips cache reads, so a URL can both fail its refetch and
    // still have a last-known-good copy in the cache, which the merge above
    // inlined. Keeping that copy is the right call, but it must not pass
    // for a fresh one. Without --refresh a failed URL is never in the cache
    // (the worker reads it before fetching), so this finds nothing.
    if args.refresh {
        let mut have_cached: HashMap<&str, bool> = HashMap::new();
        for note in &paged.notes {
            let Some(url) = note.boundary_url.as_deref() else {
                continue;
            };
            let Some(reason) = summary.failures.get(url) else {
                continue;
            };
            let cached = *have_cached.entry(url).or_insert_with(|| {
                cache
                    .as_ref()
                    .is_some_and(|c| matches!(c.read(url), Ok(Some(_))))
            });
            if cached {
                report.add(
                    "boundary_stale_from_cache",
                    &note.id,
                    &format!("{url}: refetch failed ({reason}); cached copy used"),
                );
            }
        }
    }

    // Organizations: page and merge, no boundary phase. This runs after the
    // Location merge so a failure here still leaves a consistent Location
    // snapshot behind, with the state file untouched until both succeed.
    let org_paged = page_resources(
        &client,
        &Search::new(&args.server, args.server_kind, "Organization"),
        org_since,
        &snap.incoming_organizations(),
        &mut report,
    )?;
    eprintln!(
        "paged {} organizations over {} page(s)",
        org_paged.notes.len(),
        org_paged.pages
    );
    let org_stats = merge_file(
        &MergeFiles::organizations(snap),
        &org_paged.notes,
        &MergeOptions {
            full: org_since.is_none(),
            ..MergeOptions::default()
        },
        &mut report,
    )?;

    // Compare the server's count with the merged snapshot's. Incremental
    // runs always: deletions are invisible to a `_lastUpdated` search, so a
    // drifted snapshot is at least reported. Full runs match by construction
    // unless the server stopped issuing next links before the end of the
    // result set, which looks like a final page that is exactly full (HAPI
    // FHIR 8.12 with its stock prefetch thresholds went quiet after 3,000 of
    // 51,843 Locations, 2026-09-06); only then is the count worth a request.
    for (resource_type, snapshot_total, incremental, suspicious) in [
        ("Location", stats.total, !full, paged.last_page_full),
        (
            "Organization",
            org_stats.total,
            org_since.is_some(),
            org_paged.last_page_full,
        ),
    ] {
        if !incremental && !suspicious {
            continue;
        }
        match check_count(
            &client,
            &args.server,
            args.server_kind,
            resource_type,
            snapshot_total,
        ) {
            CountCheck::Differs(server_total) => {
                let detail = if incremental {
                    format!(
                        "the server has {server_total} {resource_type} resources but the snapshot has {snapshot_total}; \
                         resources were probably deleted on the server, which an incremental extract cannot see. \
                         Run fhir-pipes extract --full to resynchronise."
                    )
                } else {
                    format!(
                        "the server reports {server_total} {resource_type} resources but the full extract paged {snapshot_total}; \
                         the server stopped paging early (no next link) or its count is stale. \
                         The snapshot is incomplete: check the server's paging limits before using it."
                    )
                };
                report.add("count_mismatch", resource_type, &detail)
            }
            CountCheck::Same => {}
            CountCheck::Unavailable(why) => {
                eprintln!("count check unavailable for {resource_type}: {why}")
            }
        }
    }

    let new_state = State {
        server: args.server.trim_end_matches('/').to_string(),
        watermark: capped_watermark(stats.watermark.clone(), paged.started.as_deref()),
        count: stats.total,
        organization_watermark: capped_watermark(
            org_stats.watermark.clone(),
            org_paged.started.as_deref(),
        ),
        organization_count: Some(org_stats.total),
        fhir_pipes_version: env!("CARGO_PKG_VERSION").to_string(),
        completed_at: format_utc(std::time::SystemTime::now()),
    };
    new_state.write(&snap.state_path())?;
    // Unlike transform, extract does not clear a stale report first: the
    // merge is atomic, so until it succeeds the snapshot on disk is still
    // the previous run's, and so is the report that describes it.
    write_report(&snap.report_path(), &report)?;

    // "updated" counts rows the server returned that the snapshot already
    // held. The watermark comparison is `ge`, so the row that set the
    // watermark is refetched by every incremental run and counted here.
    println!(
        "snapshot: {} resources, {} new, {} updated, watermark {}",
        stats.total,
        stats.added,
        stats.updated,
        stats.watermark.as_deref().unwrap_or("none"),
    );
    println!(
        "organizations: {} resources, {} new, {} updated, watermark {}",
        org_stats.total,
        org_stats.added,
        org_stats.updated,
        org_stats.watermark.as_deref().unwrap_or("none"),
    );
    println!("{}", report.summary());
    Ok(())
}
