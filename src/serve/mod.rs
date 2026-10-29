//! `fhir-pipes serve`: start refreshes over HTTP or on a schedule, keep a
//! record of each run, and tell configured destinations when a run ends.
//! See docs/serve.md and docs/superpowers/specs/2026-10-21-serve-design.md.
//!
//! One `serve` looks after one warehouse, and holds `.serve/serve.lock` in
//! it for its whole life. Each attempt of a run is a child
//! `fhir-pipes refresh` in a process group of its own.

pub mod api;
pub mod config;
pub mod metrics;
pub mod notify;
pub mod output;
pub mod query;
pub mod runner;
pub mod schedule;
pub mod secrets;
pub mod store;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::error::usage;

#[derive(clap::Args, Debug, Clone)]
pub struct ServeArgs {
    /// Config file (default: ./fhir-pipes.toml, else ./fhir-warehouse.toml).
    #[arg(long, env = "FHIR_PIPES_CONFIG")]
    config: Option<PathBuf>,
    /// Warehouse directory.
    #[arg(long, env = "FHIR_PIPES_DATA")]
    data: Option<PathBuf>,
    /// Address to listen on. Anything but a loopback address needs
    /// FHIR_PIPES_SERVE_TOKEN, and FHIR_PIPES_QUERY_TOKEN with [serve.query].
    #[arg(
        long,
        env = "FHIR_PIPES_SERVE_LISTEN",
        default_value = "127.0.0.1:8090"
    )]
    listen: String,
}

/// The token callers send as `Authorization: Bearer <token>`.
const TOKEN_ENV: &str = "FHIR_PIPES_SERVE_TOKEN";
/// The token that opens the `/fhir` paths only.
const QUERY_TOKEN_ENV: &str = "FHIR_PIPES_QUERY_TOKEN";

#[derive(Clone)]
struct App {
    token: Option<String>,
    query_token: Option<String>,
    /// Whether `[serve.query]` is set: without it, `/fhir` paths are 404.
    query_enabled: bool,
}

/// What the query API is built from, loaded at startup.
struct QueryParts {
    settings: crate::warehouse::config::Settings,
    approve: query::config::Approve,
    queries: query::catalog::Queries,
    runner: query::run::Runner,
}

/// The warehouse settings as `refresh` reads them from the same config,
/// over the warehouse serve looks after, and the catalog and runner built
/// from them.
fn query_parts(
    inputs: &crate::warehouse::ServeInputs,
    file: crate::warehouse::config::File,
    cfg: &query::config::QueryConfig,
) -> Result<QueryParts> {
    let settings = crate::warehouse::config::settings(
        file,
        inputs.base.clone(),
        crate::warehouse::serve_overrides(inputs.data.clone())?,
    )?;
    let queries = query::catalog::Queries::load(&settings, &cfg.approve)
        .map_err(|e| anyhow::anyhow!("{}", query::catalog::load_error(&e, &settings)))?;
    let runner =
        query::run::Runner::new(cfg, &inputs.data, &settings.duckdb, &settings.strip_prefix)?;
    Ok(QueryParts {
        settings,
        approve: cfg.approve.clone(),
        queries,
        runner,
    })
}

/// The pieces of a serve that the API and the schedule share.
fn server(
    inputs: &crate::warehouse::ServeInputs,
    cfg: config::ServeConfig,
    query: Option<QueryParts>,
) -> Result<Arc<api::Server>> {
    let store = Arc::new(store::Store::open(inputs.data.join(".serve/runs"))?);
    let (redactor, warnings) =
        secrets::Redactor::from_env(&cfg, inputs.database_url_env.as_deref());
    for w in warnings {
        eprintln!("fhir-pipes: warning: {w}");
    }
    let redactor = Arc::new(redactor);
    let stdout = output::StdoutQueue::start(Box::new(std::io::stdout()));
    let metrics = Arc::new(metrics::Metrics::default());
    let mut base_args = vec![
        "refresh".to_string(),
        "--config".to_string(),
        inputs.config.display().to_string(),
    ];
    // The warehouse serve locked, whichever of --data, FHIR_PIPES_DATA and
    // the config named it, so the refreshes write the same one.
    base_args.extend(["--data".to_string(), inputs.data.display().to_string()]);
    interrupt_unfinished(&store, &cfg, &stdout)?;
    let notifier = notify::Notifier::start(
        store.clone(),
        cfg.notify.clone(),
        redactor.clone(),
        notify::Times::default(),
        stdout.clone(),
        Some(metrics.clone()),
    );
    // The secrets the config names, for the refreshes to hide before they
    // cut an error short; a notify URL is given to them in a variable of
    // its own.
    let mut child_env = Vec::new();
    let mut secret_vars: Vec<String> = inputs.database_url_env.iter().cloned().collect();
    for (i, d) in cfg.notify.iter().enumerate() {
        if let Some((_, env)) = &d.header {
            secret_vars.push(env.clone());
        }
        let name = format!("FHIR_PIPES_SERVE_NOTIFY_URL_{i}");
        child_env.push((name.clone(), d.url.to_string()));
        secret_vars.push(name);
    }
    child_env.push((
        crate::redact::SECRET_VARS_ENV.to_string(),
        secret_vars.join(","),
    ));
    let query = query.map(|q| {
        // A query's duckdb gets none of the secrets a refresh is told of,
        // nor the tokens, which an approved query could read with `getenv`.
        let mut runner = q.runner;
        runner.hide_env(secret_vars.iter().cloned());
        Arc::new(query::http::QueryApi {
            queries: RwLock::new(Arc::new(q.queries)),
            runner,
            settings: q.settings,
            approve: q.approve,
            metrics: metrics.clone(),
            stdout: stdout.clone(),
            redactor: redactor.clone(),
            reloading: Default::default(),
        })
    });
    let job = Arc::new(runner::Job {
        program: std::env::current_exe().context("finding the fhir-pipes binary")?,
        base_args,
        cfg,
        store,
        redactor,
        stdout,
        write_failing: Arc::default(),
        child_env,
    });
    Ok(Arc::new(api::Server {
        job,
        notifier,
        metrics,
        data: inputs.data.clone(),
        current: std::sync::Mutex::new(None),
        clock: schedule::clock(),
        stopping: Default::default(),
        query,
    }))
}

pub fn run(args: &ServeArgs) -> Result<()> {
    let (inputs, file) =
        crate::warehouse::serve_inputs(args.config.as_deref(), args.data.clone()).map_err(usage)?;
    let cfg = config::parse(inputs.serve.clone(), &inputs.base).map_err(usage)?;
    let listen: SocketAddr = args
        .listen
        .parse()
        .with_context(|| format!("--listen {}: not an address and port", args.listen))
        .map_err(usage)?;
    let token = std::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty());
    if token.is_none() && !listen.ip().is_loopback() {
        return Err(usage(anyhow::anyhow!(
            "--listen {listen} is not a loopback address: set {TOKEN_ENV} so callers need a token"
        )));
    }
    let query_token = std::env::var(QUERY_TOKEN_ENV)
        .ok()
        .filter(|t| !t.is_empty());
    if cfg.query.is_some() && query_token.is_none() && !listen.ip().is_loopback() {
        return Err(usage(anyhow::anyhow!(
            "--listen {listen} is not a loopback address: with [serve.query], set \
             {QUERY_TOKEN_ENV} so query callers need a token"
        )));
    }
    let query = match &cfg.query {
        // The error can quote a Library source's URL, token and all.
        Some(q) => Some(query_parts(&inputs, file, q).map_err(|e| {
            usage(anyhow::anyhow!(
                "{}",
                crate::redact::urls(&crate::error::message(&e))
            ))
        })?),
        None => None,
    };
    let dir = inputs.data.join(".serve");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let _lock = lock(&dir.join("serve.lock"))?;
    let query_enabled = cfg.query.is_some();
    let server = server(&inputs, cfg, query)?;
    if !server.job.cfg.schedule.is_empty() {
        let s = server.clone();
        std::thread::spawn(move || {
            schedule::run(
                &s.job.cfg.schedule,
                &*s.clock,
                |at, mode| {
                    let request = store::RunRequest {
                        mode,
                        ..Default::default()
                    };
                    match s.start(store::Trigger::Schedule, request) {
                        Ok(_) => schedule::Fired::Started,
                        Err(api::Refused::Busy(id)) => {
                            s.metrics.schedule_skipped();
                            s.job.stdout.push(format!(
                                "schedule {}: skipped, run {} is under way",
                                at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                                id.as_str()
                            ));
                            schedule::Fired::Skipped
                        }
                        Err(api::Refused::Unrecorded(e)) => {
                            eprintln!("fhir-pipes: error: a scheduled run was not started: {e}");
                            schedule::Fired::Refused
                        }
                        Err(api::Refused::Stopping) => schedule::Fired::Refused,
                    }
                },
                None,
            );
        });
    }

    let app = App {
        token,
        query_token,
        query_enabled,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the server runtime")?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .with_context(|| format!("listening on {listen}"))?;
        let addr = listener.local_addr()?;
        // Straight to stdout, before any run: a reader waits for it. A closed
        // stdout is not an error.
        {
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = writeln!(out, "listening on http://{addr}");
            let _ = out.flush();
        }
        axum::serve(listener, router(app, server.clone()))
            .with_graceful_shutdown(shutdown(server))
            .await
            .context("serving")
    })
}

fn router(app: App, server: Arc<api::Server>) -> Router {
    let mut routes = api::routes(server.clone());
    if let Some(q) = &server.query {
        routes = routes.merge(query::http::routes(q.clone()));
    }
    routes
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(middleware::from_fn_with_state(app, auth))
        .route("/health", get(|| async { "ok" }))
}

/// Runs a previous serve left accepted, running or waiting are over: they
/// are marked interrupted with their destinations pending, so the notifier
/// sends them. No process is signalled: a process group number in an old
/// record says nothing about who owns it now.
///
/// A run whose end cannot be written stops serve here, before any run is
/// accepted: it would otherwise stay `running`, never notified.
fn interrupt_unfinished(
    store: &store::Store,
    cfg: &config::ServeConfig,
    out: &output::StdoutQueue,
) -> Result<()> {
    let runs = store.unfinished()?;
    for mut rec in runs {
        // An attempt that was under way may have left its result file, which
        // is the child's and not redacted.
        if let Ok(entries) = std::fs::read_dir(store.run_dir(&rec.id)) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with("attempt-") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        rec.status = store::RunStatus::Interrupted;
        rec.finished_at = Some(runner::now());
        for d in &cfg.notify {
            rec.notify.insert(
                d.name.clone(),
                store::Delivery {
                    state: store::DeliveryState::Pending,
                    tries: 0,
                    last_error: None,
                },
            );
        }
        store.save(&rec).with_context(|| {
            format!(
                "recording that run {} was interrupted by the last serve",
                rec.id.as_str()
            )
        })?;
        out.push(format!(
            "{} interrupted: serve stopped during it",
            rec.id.as_str()
        ));
    }
    Ok(())
}

/// Wait for SIGTERM or Ctrl-C, then stop: running queries are stopped and
/// new ones refused; the current run is interrupted, given `stop_grace`,
/// and its end written; the notifier gets the rest of the time, and
/// anything not sent goes at the next start.
async fn shutdown(server: Arc<api::Server>) {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        Err(e) => {
            eprintln!("fhir-pipes: warning: cannot handle SIGTERM: {e}");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    // Before anything else, and without writing to stdout, which may be
    // blocked: no run is accepted from now on.
    server
        .stopping
        .store(true, std::sync::atomic::Ordering::SeqCst);
    server.job.stdout.push("stopping: no new runs".into());
    let _ = tokio::task::spawn_blocking(move || stop(&server)).await;
}

fn stop(server: &api::Server) {
    if let Some(q) = &server.query {
        q.runner.stop(query::run::STOP_GRACE);
    }
    let grace = server.job.cfg.stop_grace;
    let deadline = std::time::Instant::now() + grace + std::time::Duration::from_secs(5);
    let current = server
        .current
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|(id, token)| (id.clone(), token.clone()));
    let Some((id, token)) = current else {
        return;
    };
    server
        .job
        .stdout
        .push(format!("{} interrupted: serve is stopping", id.as_str()));
    token.interrupt(grace);
    let waiting = |done: &dyn Fn() -> bool| {
        while !done() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    // The run's thread writes its end, then clears the current run; the
    // notifier then has the rest of the time.
    waiting(&|| {
        server
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
    });
    waiting(&|| !server.job.store.load(&id).is_ok_and(|r| r.pending()));
}

/// Every path but /health needs a token, checked before anything else
/// about the request is read. The `/fhir` paths take the query token or the
/// serve token, the rest the serve token only; with none of the tokens a
/// path takes set, it is open (serve then listens only on loopback).
async fn auth(State(app): State<App>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let fhir = path == "/fhir" || path.starts_with("/fhir/");
    let accepted: Vec<&String> = if fhir {
        app.query_token.iter().chain(&app.token).collect()
    } else {
        app.token.iter().collect()
    };
    if !accepted.is_empty() {
        let given = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        // Each token is compared, so the time taken does not say which
        // one matched.
        let matched = given.is_some_and(|g| {
            accepted
                .iter()
                .fold(false, |m, t| same(g.as_bytes(), t.as_bytes()) | m)
        });
        if !matched {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    // Only now, so an unauthenticated caller cannot tell whether the query
    // API is enabled.
    if fhir && !app.query_enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(req).await
}

/// Constant-time comparison: the time taken does not depend on where the
/// two differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Take `path` exclusively for the life of the process, or fail at once.
fn lock(path: &std::path::Path) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => bail!(
            "another fhir-pipes serve looks after this warehouse: it holds {}",
            path.display()
        ),
        Err(fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("locking {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_compares_whole_values() {
        assert!(same(b"token", b"token"));
        assert!(!same(b"token", b"tokens"));
        assert!(!same(b"token", b"tokem"));
        assert!(!same(b"", b"x"));
    }
}
