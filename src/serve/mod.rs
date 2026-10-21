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
pub mod runner;
pub mod schedule;
pub mod secrets;
pub mod store;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

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
    /// FHIR_PIPES_SERVE_TOKEN.
    #[arg(
        long,
        env = "FHIR_PIPES_SERVE_LISTEN",
        default_value = "127.0.0.1:8090"
    )]
    listen: String,
}

/// The token callers send as `Authorization: Bearer <token>`.
const TOKEN_ENV: &str = "FHIR_PIPES_SERVE_TOKEN";

#[derive(Clone)]
struct App {
    token: Option<String>,
}

/// The pieces of a serve that the API and the schedule share.
fn server(
    inputs: &crate::warehouse::ServeInputs,
    data_flag: Option<&PathBuf>,
    cfg: config::ServeConfig,
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
    if let Some(d) = data_flag {
        base_args.extend(["--data".to_string(), d.display().to_string()]);
    }
    interrupt_unfinished(&store, &cfg);
    let notifier = notify::Notifier::start(
        store.clone(),
        cfg.notify.clone(),
        redactor.clone(),
        notify::Times::default(),
        stdout.clone(),
        Some(metrics.clone()),
    );
    let job = Arc::new(runner::Job {
        program: std::env::current_exe().context("finding the fhir-pipes binary")?,
        base_args,
        cfg,
        store,
        redactor,
        stdout,
        write_failing: Arc::default(),
    });
    Ok(Arc::new(api::Server {
        job,
        notifier,
        metrics,
        data: inputs.data.clone(),
        current: std::sync::Mutex::new(None),
        clock: schedule::clock(),
    }))
}

pub fn run(args: &ServeArgs) -> Result<()> {
    let inputs =
        crate::warehouse::serve_inputs(args.config.as_deref(), args.data.clone()).map_err(usage)?;
    let cfg = config::parse(inputs.serve.clone()).map_err(usage)?;
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
    let dir = inputs.data.join(".serve");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let _lock = lock(&dir.join("serve.lock"))?;
    // FHIR_PIPES_DATA reaches the refreshes through the environment; a
    // --data flag is passed on.
    let data_flag = args
        .data
        .as_ref()
        .filter(|_| std::env::var_os("FHIR_PIPES_DATA").is_none());
    let server = server(&inputs, data_flag, cfg)?;
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
                    }
                },
                None,
            );
        });
    }

    let app = App { token };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the server runtime")?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .with_context(|| format!("listening on {listen}"))?;
        let addr = listener.local_addr()?;
        println!("listening on http://{addr}");
        axum::serve(listener, router(app, server.clone()))
            .with_graceful_shutdown(shutdown(server))
            .await
            .context("serving")
    })
}

fn router(app: App, server: Arc<api::Server>) -> Router {
    api::routes(server)
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(middleware::from_fn_with_state(app, auth))
        .route("/health", get(|| async { "ok" }))
}

/// Runs a previous serve left accepted, running or waiting are over: they
/// are marked interrupted with their destinations pending, so the notifier
/// sends them. No process is signalled: a process group number in an old
/// record says nothing about who owns it now.
fn interrupt_unfinished(store: &store::Store, cfg: &config::ServeConfig) {
    let runs = match store.unfinished() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fhir-pipes: warning: {}", crate::error::message(&e));
            return;
        }
    };
    for mut rec in runs {
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
        match store.save(&rec) {
            Ok(()) => println!("{} interrupted: serve stopped during it", rec.id.as_str()),
            Err(e) => eprintln!("fhir-pipes: warning: {}", crate::error::message(&e)),
        }
    }
}

/// Wait for SIGTERM or Ctrl-C, then stop: the current run is interrupted,
/// given `stop_grace`, and its end written; the notifier gets the rest of
/// the time, and anything not sent goes at the next start.
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
    println!("stopping: no new runs");
    let _ = tokio::task::spawn_blocking(move || stop(&server)).await;
}

fn stop(server: &api::Server) {
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
    println!("{} interrupted: serve is stopping", id.as_str());
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

/// Every path but /health needs the token, checked before anything else
/// about the request is read.
async fn auth(State(app): State<App>, req: Request, next: Next) -> Response {
    if let Some(token) = &app.token {
        let given = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if !given.is_some_and(|g| same(g.as_bytes(), token.as_bytes())) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
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
