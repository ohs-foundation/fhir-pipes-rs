//! `fhir-pipes serve`: start refreshes over HTTP or on a schedule, keep a
//! record of each run, and tell configured destinations when a run ends.
//! See docs/serve.md and docs/superpowers/specs/2026-10-21-serve-design.md.
//!
//! One `serve` looks after one warehouse, and holds `.serve/serve.lock` in
//! it for its whole life. Each attempt of a run is a child
//! `fhir-pipes refresh` in a process group of its own.

// Parts are used by the tasks that follow; removed once serve is whole.
#![allow(dead_code)]

pub mod config;
pub mod store;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

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

pub fn run(args: &ServeArgs) -> Result<()> {
    let inputs =
        crate::warehouse::serve_inputs(args.config.as_deref(), args.data.clone()).map_err(usage)?;
    let _cfg = config::parse(inputs.serve.clone()).map_err(usage)?;
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
        axum::serve(listener, router(app)).await.context("serving")
    })
}

fn router(app: App) -> Router {
    Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .route("/health", get(|| async { "ok" }))
        .with_state(app)
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
