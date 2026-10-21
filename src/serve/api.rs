//! The HTTP API: start a run, see runs and their logs, cancel one, metrics.
//! Every path but /health needs the token (mod.rs checks it before a body
//! is read or a run id looked up).

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use super::metrics::Metrics;
use super::notify::Notifier;
use super::runner::{self, CancelToken, Job};
use super::store::{RunId, RunRecord, RunRequest, Trigger};

/// The steps a request may name in `only` and `skip`, as `refresh` takes them.
const STEPS: [&str; 8] = [
    "locations",
    "views",
    "tiles",
    "manifest",
    "catalog",
    "database",
    "hooks",
    "push",
];
const MAX_BODY: usize = 16 * 1024;
const MAX_LIMIT: usize = 500;
const MAX_TAIL: usize = 10_000;

pub struct Server {
    pub job: Arc<Job>,
    pub notifier: Notifier,
    pub metrics: Arc<Metrics>,
    pub data: PathBuf,
    /// The run accepted, running or waiting, if any: one at a time, decided
    /// under this lock by the API and the schedule alike.
    pub current: Mutex<Option<(RunId, CancelToken)>>,
}

/// Why a run was not started.
pub enum Refused {
    /// Another run is current.
    Busy(RunId),
    /// Its record could not be written.
    Unrecorded(String),
}

impl Server {
    fn lock_current(&self) -> std::sync::MutexGuard<'_, Option<(RunId, CancelToken)>> {
        self.current.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Accept a run, write its record, and start it on a thread of its own.
    pub fn start(
        self: &Arc<Self>,
        trigger: Trigger,
        request: RunRequest,
    ) -> Result<RunId, Refused> {
        let mut current = self.lock_current();
        if let Some((id, _)) = current.as_ref() {
            return Err(Refused::Busy(id.clone()));
        }
        if self.job.write_failing.load(Ordering::SeqCst) {
            return Err(Refused::Unrecorded(
                "a run's record cannot be written; see serve's stderr".into(),
            ));
        }
        let mut rec = RunRecord::new(
            RunId::new(chrono::Utc::now()),
            trigger,
            request,
            runner::now(),
        );
        if let Err(e) = self.job.store.create(&mut rec) {
            return Err(Refused::Unrecorded(crate::error::message(&e)));
        }
        let id = rec.id.clone();
        let cancel = CancelToken::default();
        *current = Some((id.clone(), cancel.clone()));
        drop(current);
        self.job.stdout.push(format!(
            "{} accepted ({})",
            id.as_str(),
            super::metrics::name(&trigger)
        ));
        self.metrics.running.store(true, Ordering::SeqCst);
        let server = self.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let rec = runner::run(&server.job, rec, &cancel);
            server
                .metrics
                .run_ended(&rec, started.elapsed().as_secs_f64());
            server.job.stdout.push(format!(
                "{} ended: {}",
                rec.id.as_str(),
                super::metrics::name(&rec.status)
            ));
            server.notifier.enqueue(rec.id.clone());
            if let Err(e) = server.job.store.prune(server.job.cfg.keep_runs) {
                eprintln!("fhir-pipes: warning: {}", crate::error::message(&e));
            }
            server.metrics.running.store(false, Ordering::SeqCst);
            *server.lock_current() = None;
        });
        Ok(id)
    }

    fn current_id(&self) -> Option<RunId> {
        self.lock_current().as_ref().map(|(id, _)| id.clone())
    }
}

pub fn routes(server: Arc<Server>) -> axum::Router {
    axum::Router::new()
        .route(
            "/run",
            post(start_run).layer(DefaultBodyLimit::max(MAX_BODY)),
        )
        .route("/status", get(status))
        .route("/runs", get(runs))
        .route("/runs/{id}", get(run))
        .route("/runs/{id}/log", get(log))
        .route("/runs/{id}/cancel", post(cancel))
        .route("/metrics", get(metrics))
        .with_state(server)
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

async fn start_run(State(server): State<Arc<Server>>, body: Bytes) -> Response {
    let request: RunRequest = if body.iter().all(u8::is_ascii_whitespace) {
        RunRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => return error(StatusCode::BAD_REQUEST, format!("the request: {e}")),
        }
    };
    if let Some(bad) = request
        .only
        .iter()
        .chain(&request.skip)
        .find(|s| !STEPS.contains(&s.as_str()))
    {
        return error(
            StatusCode::BAD_REQUEST,
            format!("unknown step {bad:?}; the steps are {}", STEPS.join(", ")),
        );
    }
    let started = tokio::task::spawn_blocking(move || server.start(Trigger::Api, request)).await;
    match started {
        Ok(Ok(id)) => (
            StatusCode::ACCEPTED,
            Json(json!({"id": id, "status_url": format!("/runs/{}", id.as_str())})),
        )
            .into_response(),
        Ok(Err(Refused::Busy(id))) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "a run is under way", "current": id})),
        )
            .into_response(),
        Ok(Err(Refused::Unrecorded(e))) => error(StatusCode::SERVICE_UNAVAILABLE, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// A record without its attempts' results, for lists.
fn summary(rec: &RunRecord) -> Value {
    let mut v = serde_json::to_value(rec).unwrap_or(Value::Null);
    if let Some(attempts) = v["attempts"].as_array_mut() {
        for a in attempts {
            if let Some(o) = a.as_object_mut() {
                o.remove("result");
            }
        }
    }
    v
}

async fn status(State(server): State<Arc<Server>>) -> Response {
    let s = server.clone();
    let got = tokio::task::spawn_blocking(move || {
        let current = s.current_id().and_then(|id| s.job.store.load(&id).ok());
        let last = s
            .job
            .store
            .list(MAX_LIMIT)
            .ok()
            .and_then(|runs| runs.into_iter().find(|r| r.status.finished()));
        json!({
            "current": current.as_ref().map(summary),
            "last": last.as_ref().map(summary),
            "next_scheduled": Value::Null,
        })
    })
    .await;
    match got {
        Ok(v) => Json(v).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct Limit {
    limit: Option<usize>,
}

async fn runs(State(server): State<Arc<Server>>, Query(q): Query<Limit>) -> Response {
    let limit = q.limit.unwrap_or(50).min(MAX_LIMIT);
    match tokio::task::spawn_blocking(move || server.job.store.list(limit)).await {
        Ok(Ok(runs)) => Json(runs.iter().map(summary).collect::<Vec<_>>()).into_response(),
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, crate::error::message(&e)),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "no such run")
}

async fn run(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let Some(id) = RunId::parse(&id) else {
        return not_found();
    };
    match tokio::task::spawn_blocking(move || server.job.store.load(&id)).await {
        Ok(Ok(rec)) => Json(rec).into_response(),
        _ => not_found(),
    }
}

#[derive(Deserialize)]
struct Tail {
    tail: Option<usize>,
}

async fn log(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Query(q): Query<Tail>,
) -> Response {
    let Some(id) = RunId::parse(&id) else {
        return not_found();
    };
    let path = server.job.store.run_dir(&id).join("log");
    if !server.job.store.run_dir(&id).join("run.json").is_file() {
        return not_found();
    }
    let got = tokio::task::spawn_blocking(move || match q.tail {
        Some(n) => super::output::tail(&path, n.min(MAX_TAIL)),
        None => std::fs::read_to_string(&path).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(String::new())
            } else {
                Err(e)
            }
        }),
    })
    .await;
    match got {
        Ok(Ok(text)) => {
            ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
        }
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn cancel(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let Some(id) = RunId::parse(&id) else {
        return not_found();
    };
    let token = server
        .lock_current()
        .as_ref()
        .filter(|(current, _)| *current == id)
        .map(|(_, token)| token.clone());
    if let Some(token) = token {
        token.cancel(server.job.cfg.stop_grace);
        return Json(json!({"id": id, "status": "cancelling"})).into_response();
    }
    if server.job.store.run_dir(&id).join("run.json").is_file() {
        return error(StatusCode::CONFLICT, "the run has ended");
    }
    not_found()
}

async fn metrics(State(server): State<Arc<Server>>) -> Response {
    let destinations: Vec<String> = server
        .job
        .cfg
        .notify
        .iter()
        .map(|d| d.name.clone())
        .collect();
    let text = server.metrics.render(
        &destinations,
        &server.data.join("manifest.json"),
        server.job.write_failing.load(Ordering::SeqCst),
        server.job.stdout.dropped(),
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], text).into_response()
}
