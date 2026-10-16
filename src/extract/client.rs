//! The one HTTP client. Bearer auth, FHIR accept header, and the retry
//! policy shared by paging, boundary fetches and load's bundle posts:
//! connection errors, 5xx and 429 are retried with exponential backoff
//! (0.5 s doubling, clamped to [0, 30] s, a numeric Retry-After replacing
//! the computed delay); any other status is final.
//!
//! The bearer token is either fixed (`--token`), obtained with the OAuth
//! client-credentials grant (`--client-id`), or obtained from Google
//! Application Default Credentials (`--google-auth`, see `google.rs`). In the
//! last two cases the client gets a new token shortly before the current one
//! expires, so a run can outlast any one token.
//!
//! `fhir-pipes dhis2` uses the same client against the DHIS2 Web API, with a
//! complete Authorization value (`ApiToken …` or `Basic …`) and plain JSON.
//!
//! `export --bulk` uses it for the Bulk Data kick-off, the status URL (whose
//! 202 and 429 the caller reads itself) and the output files.

use std::io::Read as _;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use reqwest::Method;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::Value;

use super::google::{self, Credentials};
use crate::error::{PipesError, Result};

const RETRY_BASE: f64 = 0.5;
const RETRY_MAX: f64 = 30.0;
const ERROR_BODY_CAP: usize = 500;
/// Never read more than this much of an error body off the wire; we only
/// keep the first ERROR_BODY_CAP chars of it anyway.
const ERROR_BODY_READ_LIMIT: u64 = 8192;
/// How much of a non-2xx body `exchange` keeps whole, for a caller that
/// parses it (a Bulk Data OperationOutcome). Beyond it the body is cut and
/// `truncated` set.
const EXCHANGE_BODY_LIMIT: u64 = 1 << 20;
/// Assumed token lifetime, in seconds, when a token response omits `expires_in`.
const DEFAULT_TOKEN_LIFETIME: u64 = 300;
/// Per-request timeout for the Google metadata server, which answers in
/// milliseconds on Google Cloud; short so a run elsewhere fails quickly.
const METADATA_TIMEOUT: Duration = Duration::from_secs(3);

/// How requests are authorised.
#[derive(Clone)]
pub enum Auth {
    None,
    /// A fixed bearer token.
    Token(String),
    /// A complete Authorization header value, e.g. `ApiToken d2pat_…` for a
    /// DHIS2 personal access token or `Basic …`.
    Header(String),
    /// OAuth 2.0 client credentials, traded for bearer tokens at `token_url`.
    ClientCredentials {
        token_url: String,
        client_id: String,
        client_secret: String,
    },
    /// Google credentials, usually `Credentials::application_default()`.
    Google(Credentials),
}

impl Auth {
    /// HTTP Basic: `Basic base64(user:password)`.
    pub fn basic(user: &str, password: &str) -> Self {
        use base64::Engine;
        Auth::Header(format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
        ))
    }
}

/// The current renewable token and when to replace it. The lock is held
/// across a renewal so concurrent boundary workers wait for one new token
/// instead of each fetching their own.
struct TokenSource {
    grant: Grant,
    current: Mutex<(HeaderValue, Instant)>,
}

/// How a token is requested.
enum Grant {
    /// POST a fixed form: OAuth client credentials, or a Google refresh token.
    Form { url: String, form: String },
    /// POST a newly signed JWT (a Google service account key).
    Jwt(google::ServiceAccount),
    /// GET from the Google metadata server.
    Metadata { url: String },
}

impl Grant {
    fn url(&self) -> &str {
        match self {
            Grant::Form { url, .. } | Grant::Metadata { url } => url,
            Grant::Jwt(sa) => &sa.token_uri,
        }
    }
}

enum Authorizer {
    None,
    Fixed(HeaderValue),
    Renewing(TokenSource),
}

/// How long to use a token that lives `expires_in` seconds: until a minute
/// before it expires, or half its life when that is two minutes or less.
fn renew_after(expires_in: u64) -> Duration {
    Duration::from_secs(if expires_in > 120 {
        expires_in - 60
    } else {
        expires_in / 2
    })
}

fn bearer(token: &str) -> Option<HeaderValue> {
    let mut v = HeaderValue::from_str(&format!("Bearer {token}")).ok()?;
    v.set_sensitive(true);
    Some(v)
}

#[derive(Debug)]
pub enum FetchError {
    /// Final status after retries; `body` is at most ERROR_BODY_CAP chars of the response.
    Status { status: u16, body: String },
    /// Connection, TLS, timeout or protocol error after retries.
    Transport(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Status { status, body } if body.is_empty() => write!(f, "HTTP {status}"),
            FetchError::Status { status, body } => write!(f, "HTTP {status}: {body}"),
            FetchError::Transport(msg) => write!(f, "{msg}"),
        }
    }
}

pub struct Fetched {
    pub body: Vec<u8>,
    /// The response's HTTP `Date` header: the server's clock when it answered.
    pub date: Option<String>,
}

/// A response whose status and headers the caller reads.
pub struct Exchange {
    pub status: u16,
    pub headers: HeaderMap,
    /// The whole body of a 2xx; of any other, at most EXCHANGE_BODY_LIMIT
    /// bytes, uncapped, so the caller can parse it.
    pub body: Vec<u8>,
    /// A non-2xx body was longer than EXCHANGE_BODY_LIMIT and was cut.
    pub truncated: bool,
}

impl Exchange {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

impl std::fmt::Debug for Exchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Exchange {{ status: {}, body: <{} bytes> }}",
            self.status,
            self.body.len()
        )
    }
}

/// A 2xx response's body and `Date`.
fn fetched(resp: Response) -> std::result::Result<Fetched, FetchError> {
    let date = resp
        .headers()
        .get(reqwest::header::DATE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match resp.bytes() {
        Ok(b) => Ok(Fetched {
            body: b.to_vec(),
            date,
        }),
        Err(e) => Err(FetchError::Transport(format!("reading body: {e}"))),
    }
}

/// Any response, with its status and headers.
fn exchanged(resp: Response) -> std::result::Result<Exchange, FetchError> {
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let mut truncated = false;
    let body = if (200..300).contains(&status) {
        resp.bytes()
            .map_err(|e| FetchError::Transport(format!("reading body: {e}")))?
            .to_vec()
    } else {
        let mut buf = Vec::new();
        let _ = resp.take(EXCHANGE_BODY_LIMIT + 1).read_to_end(&mut buf);
        if buf.len() as u64 > EXCHANGE_BODY_LIMIT {
            buf.truncate(EXCHANGE_BODY_LIMIT as usize);
            truncated = true;
        }
        buf
    };
    Ok(Exchange {
        status,
        headers,
        body,
        truncated,
    })
}

impl std::fmt::Debug for Fetched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fetched {{ body: <{} bytes> }}", self.body.len())
    }
}

pub struct FhirClient {
    http: Client,
    retries: usize,
    /// The per-request timeout the client was built with.
    timeout: Duration,
    auth: Authorizer,
    /// `x-goog-user-project`, sent with the Authorization header.
    user_project: Option<HeaderValue>,
    /// The limit on requests in flight, when the client has one.
    gate: Option<Gate>,
}

/// A limit on the requests in flight from all the threads sharing a client
/// (`export --concurrency`), and a pause they all wait out after a response
/// with a Retry-After, so parallel downloads back off together. A request
/// holds its place from sending until its body is read, not while it waits
/// to retry.
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

struct GateState {
    limit: usize,
    in_flight: usize,
    paused_until: Option<Instant>,
}

/// A place in the gate, given back when dropped.
struct Pass<'a>(&'a Gate);

impl Gate {
    fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(GateState {
                limit: limit.max(1),
                in_flight: 0,
                paused_until: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait for a pause to end and a place to be free.
    fn enter(&self) -> Pass<'_> {
        let mut state = self.lock();
        loop {
            let now = Instant::now();
            if let Some(until) = state.paused_until.filter(|u| *u > now) {
                state = self
                    .changed
                    .wait_timeout(state, until - now)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            } else if state.in_flight < state.limit {
                state.in_flight += 1;
                return Pass(self);
            } else {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
    }

    /// No request starts for `wait`, or until an earlier pause ends.
    fn pause(&self, wait: Duration) {
        let until = Instant::now() + wait;
        let mut state = self.lock();
        if state.paused_until.is_none_or(|u| u < until) {
            state.paused_until = Some(until);
        }
    }
}

impl Drop for Pass<'_> {
    fn drop(&mut self) {
        self.0.lock().in_flight -= 1;
        // All, not one: a thread waiting out a pause must not take the
        // wake-up meant for one waiting for a place.
        self.0.changed.notify_all();
    }
}

pub fn backoff_delay(attempt: usize, retry_after: Option<f64>) -> Duration {
    let secs =
        retry_after.unwrap_or_else(|| RETRY_BASE * 2f64.powi(attempt.saturating_sub(1) as i32));
    Duration::from_secs_f64(secs.clamp(0.0, RETRY_MAX))
}

pub fn parse_retry_after(value: Option<&str>) -> Option<f64> {
    value?.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn cap(body: String) -> String {
    body.chars().take(ERROR_BODY_CAP).collect()
}

/// Read at most ERROR_BODY_READ_LIMIT bytes of an error response, then cap
/// to ERROR_BODY_CAP chars. Bounded so a server that streams gigabytes of
/// "error" doesn't make us buffer it all just to report a status code.
fn read_error_body(resp: Response) -> String {
    let mut buf = Vec::new();
    let _ = resp.take(ERROR_BODY_READ_LIMIT).read_to_end(&mut buf);
    cap(String::from_utf8_lossy(&buf).into_owned())
}

/// A crypto provider must be installed before the first `Client::builder()`
/// call: reqwest is built with `rustls-no-provider`, so building a client
/// panics at build time (not merely on the first request) if none is
/// configured. Idempotent and cheap enough to call on every `new`.
pub(crate) fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
    });
}

/// A factor in [0.5, 1.0) applied to a computed backoff delay. Eight
/// workers retrying in lockstep after a shared 429 is a self-inflicted
/// herd; spreading their sleeps out avoids re-synchronizing the retry.
/// A cheap thread-local xorshift is plenty here — this isn't cryptographic,
/// just enough spread to desynchronize concurrent retriers.
fn jitter_factor() -> f64 {
    thread_local! {
        static RNG: std::cell::Cell<u64> = std::cell::Cell::new(seed());
    }

    fn seed() -> u64 {
        let t = std::time::Instant::now();
        (t.elapsed().as_nanos() as u64) | 1
    }

    fn xorshift(mut x: u64) -> u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    }

    RNG.with(|cell| {
        let next = xorshift(cell.get());
        cell.set(next);
        (next >> 11) as f64 / (1u64 << 53) as f64
    })
}

fn jittered(delay: Duration) -> Duration {
    let factor = 0.5 + 0.5 * jitter_factor();
    Duration::from_secs_f64(delay.as_secs_f64() * factor)
}

impl FhirClient {
    /// With client credentials or Google credentials the first token is
    /// fetched here, so a wrong secret, key or token URL fails before any
    /// FHIR request is made.
    pub fn new(auth: Auth, retries: usize, timeout: Duration) -> Result<Self> {
        ensure_crypto_provider();
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/fhir+json"));
        let http = Client::builder()
            .default_headers(headers)
            .timeout(timeout)
            .user_agent(concat!("fhir-pipes/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| PipesError::Environment(format!("cannot build HTTP client: {e}")))?;
        let mut client = Self {
            http,
            retries: retries.max(1),
            timeout,
            auth: Authorizer::None,
            user_project: None,
            gate: None,
        };
        client.auth = match auth {
            Auth::None => Authorizer::None,
            Auth::Token(t) => Authorizer::Fixed(bearer(&t).ok_or_else(|| {
                PipesError::Usage("token contains characters that are not valid in a header".into())
            })?),
            Auth::Header(h) => {
                let mut v = HeaderValue::from_str(&h).map_err(|_| {
                    PipesError::Usage(
                        "credentials contain characters that are not valid in a header".into(),
                    )
                })?;
                v.set_sensitive(true);
                Authorizer::Fixed(v)
            }
            Auth::ClientCredentials {
                token_url,
                client_id,
                client_secret,
            } => {
                let form = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("grant_type", "client_credentials")
                    .append_pair("client_id", &client_id)
                    .append_pair("client_secret", &client_secret)
                    .finish();
                client.renewing(Grant::Form {
                    url: token_url,
                    form,
                })?
            }
            Auth::Google(creds) => {
                if let Some(project) = &creds.quota_project {
                    client.user_project = Some(HeaderValue::from_str(project).map_err(|_| {
                        PipesError::Usage(
                            "the Google quota project is not valid in a header".into(),
                        )
                    })?);
                }
                let grant = match &creds.source {
                    google::Source::ServiceAccount(sa) => Grant::Jwt(sa.clone()),
                    google::Source::RefreshToken { token_uri, form } => Grant::Form {
                        url: token_uri.clone(),
                        form: form.clone(),
                    },
                    google::Source::Metadata { url } => Grant::Metadata { url: url.clone() },
                };
                client.renewing(grant).map_err(|e| {
                    let hint = if creds.is_metadata() {
                        ". Off Google Cloud, set GOOGLE_APPLICATION_CREDENTIALS to a service \
                         account key file or run `gcloud auth application-default login`"
                    } else {
                        ""
                    };
                    PipesError::Environment(format!(
                        "Google credentials from {}: {e}{hint}",
                        creds.describe()
                    ))
                })?
            }
        };
        Ok(client)
    }

    /// At most `limit` requests in flight at once from all the threads using
    /// this client, and a Retry-After on any response pauses them all.
    pub fn with_concurrency(mut self, limit: usize) -> Self {
        self.gate = Some(Gate::new(limit));
        self
    }

    /// A place in the gate, when the client has one.
    fn enter(&self) -> Option<Pass<'_>> {
        self.gate.as_ref().map(Gate::enter)
    }

    /// A renewing authorizer, with its first token fetched now.
    fn renewing(&self, grant: Grant) -> Result<Authorizer> {
        let first = self.request_token(&grant).map_err(|e| {
            PipesError::Environment(format!("cannot get a token from {}: {e}", grant.url()))
        })?;
        Ok(Authorizer::Renewing(TokenSource {
            grant,
            current: Mutex::new(first),
        }))
    }

    /// GET with the retry policy. Sleeps between attempts.
    pub fn get(&self, url: &str) -> std::result::Result<Fetched, FetchError> {
        self.send(|| self.http.get(url), fetched)
    }

    /// GET a plain JSON API (DHIS2) with the retry policy. `no-cache` because
    /// DHIS2 marks analytics responses cacheable for up to two weeks, and a
    /// proxy could otherwise answer from before the last analytics run.
    pub fn get_json(&self, url: &str) -> std::result::Result<Fetched, FetchError> {
        self.send(
            || {
                self.http
                    .get(url)
                    .header(ACCEPT, "application/json")
                    .header(CACHE_CONTROL, "no-cache")
            },
            fetched,
        )
    }

    /// POST a FHIR JSON body with the same retry policy. The body is cloned
    /// per attempt; bundles are small next to a boundary fetch.
    pub fn post_json(&self, url: &str, body: Vec<u8>) -> std::result::Result<Fetched, FetchError> {
        self.send(
            || {
                self.http
                    .post(url)
                    .header(CONTENT_TYPE, "application/fhir+json")
                    .body(body.clone())
            },
            fetched,
        )
    }

    /// GET with `Prefer: respond-async`, the Bulk Data kick-off. Retried as
    /// `get` is; the 202 comes back with its headers, for `Content-Location`.
    pub fn get_async(&self, url: &str) -> std::result::Result<Exchange, FetchError> {
        self.send(
            || self.http.get(url).header("prefer", "respond-async"),
            exchanged,
        )
    }

    /// One request whose status the caller interprets: only connection
    /// errors are retried, and any response comes back, whatever its status.
    /// A Bulk Data status URL answers 202 or 429 while the export runs, and
    /// the caller does the waiting.
    pub fn exchange(&self, method: Method, url: &str) -> std::result::Result<Exchange, FetchError> {
        self.exchange_by(method, url, None)
    }

    /// `exchange`, with every attempt and the waits between them ending by
    /// `deadline`: each attempt keeps the client's own timeout, cut short
    /// when less time is left, and no retry starts after the deadline.
    pub fn exchange_by(
        &self,
        method: Method,
        url: &str,
        deadline: Option<Instant>,
    ) -> std::result::Result<Exchange, FetchError> {
        let auth = self.authorization()?;
        let left = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let mut last = None;
        for attempt in 1..=self.retries {
            let mut request = self.http.request(method.clone(), url);
            if let Some(left) = left() {
                if left.is_zero() {
                    break;
                }
                request = request.timeout(self.timeout.min(left));
            }
            if let Some(v) = &auth {
                request = self.authorize(request, v);
            }
            let pass = self.enter();
            match request.send() {
                Ok(resp) => return exchanged(resp),
                Err(e) => {
                    drop(pass);
                    last = Some(FetchError::Transport(e.to_string()));
                    if attempt < self.retries {
                        let wait = jittered(backoff_delay(attempt, None));
                        std::thread::sleep(left().map_or(wait, |l| wait.min(l)));
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| FetchError::Transport("the deadline passed".to_string())))
    }

    /// GET an NDJSON file and hand its body to `read` as it arrives, with the
    /// retry policy. A failure while reading is not retried, since `read` may
    /// have written part of the file. The Authorization header is sent only
    /// when `authorize`: a Bulk Data output kept elsewhere (a signed storage
    /// URL) must not get the server's token. `timeout` replaces the client's,
    /// for a file that takes longer than a search page should.
    pub fn download<T>(
        &self,
        url: &str,
        authorize: bool,
        timeout: Duration,
        mut read: impl FnMut(&mut dyn std::io::Read) -> std::io::Result<T>,
    ) -> std::result::Result<T, FetchError> {
        let request = || {
            self.http
                .get(url)
                .header(ACCEPT, "application/fhir+ndjson")
                .timeout(timeout)
        };
        let body = |mut resp: Response| {
            read(&mut resp).map_err(|e| FetchError::Transport(format!("reading body: {e}")))
        };
        if authorize {
            self.send(request, body)
        } else {
            self.retrying(request, body)
        }
    }

    /// One token request, with the retry policy. Returns the Authorization
    /// header and when to replace it, counted from before the request so the
    /// margin also covers the round trip.
    fn request_token(
        &self,
        grant: &Grant,
    ) -> std::result::Result<(HeaderValue, Instant), FetchError> {
        let started = Instant::now();
        let post = |url: &str, form: String| {
            self.retrying(
                || {
                    self.http
                        .post(url)
                        .header(ACCEPT, "application/json")
                        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(form.clone())
                },
                fetched,
            )
        };
        let got = match grant {
            Grant::Form { url, form } => post(url, form.clone())?,
            Grant::Jwt(sa) => post(
                &sa.token_uri,
                sa.token_form().map_err(FetchError::Transport)?,
            )?,
            Grant::Metadata { url } => self.retrying(
                || {
                    self.http
                        .get(url)
                        .header(ACCEPT, "application/json")
                        .header("metadata-flavor", "Google")
                        .timeout(METADATA_TIMEOUT)
                },
                fetched,
            )?,
        };
        let body: Value = serde_json::from_slice(&got.body)
            .map_err(|e| FetchError::Transport(format!("token response is not JSON: {e}")))?;
        let header = body
            .get("access_token")
            .and_then(Value::as_str)
            .and_then(bearer)
            .ok_or_else(|| {
                FetchError::Transport("token response has no usable access_token".into())
            })?;
        let expires_in = body
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TOKEN_LIFETIME);
        Ok((header, started + renew_after(expires_in)))
    }

    /// The Authorization header for the next request, renewing a
    /// client-credentials token that is due.
    fn authorization(&self) -> std::result::Result<Option<HeaderValue>, FetchError> {
        match &self.auth {
            Authorizer::None => Ok(None),
            Authorizer::Fixed(v) => Ok(Some(v.clone())),
            Authorizer::Renewing(src) => {
                let mut current = src.current.lock().unwrap_or_else(PoisonError::into_inner);
                if Instant::now() >= current.1 {
                    *current = self.request_token(&src.grant).map_err(|e| {
                        FetchError::Transport(format!(
                            "renewing the token at {}: {e}",
                            src.grant.url()
                        ))
                    })?;
                }
                Ok(Some(current.0.clone()))
            }
        }
    }

    /// The retry loop with the Authorization header added. The header is
    /// chosen once per call, so every attempt carries the same token.
    fn send<T>(
        &self,
        request: impl Fn() -> RequestBuilder,
        success: impl FnOnce(Response) -> std::result::Result<T, FetchError>,
    ) -> std::result::Result<T, FetchError> {
        let auth = self.authorization()?;
        self.retrying(
            || match &auth {
                Some(v) => self.authorize(request(), v),
                None => request(),
            },
            success,
        )
    }

    /// The Authorization header, and `x-goog-user-project` when set.
    fn authorize(&self, request: RequestBuilder, auth: &HeaderValue) -> RequestBuilder {
        let request = request.header(AUTHORIZATION, auth.clone());
        match &self.user_project {
            Some(p) => request.header("x-goog-user-project", p.clone()),
            None => request,
        }
    }

    /// The retry loop. Any 2xx is a success, handed to `success`; 429 and
    /// 5xx retry with backoff (a numeric Retry-After replaces the computed
    /// delay); any other status is final on the first response.
    fn retrying<T>(
        &self,
        request: impl Fn() -> RequestBuilder,
        success: impl FnOnce(Response) -> std::result::Result<T, FetchError>,
    ) -> std::result::Result<T, FetchError> {
        let mut last: Option<FetchError> = None;
        for attempt in 1..=self.retries {
            let pass = self.enter();
            match request().send() {
                Err(e) => {
                    drop(pass);
                    last = Some(FetchError::Transport(e.to_string()));
                    if attempt < self.retries {
                        std::thread::sleep(jittered(backoff_delay(attempt, None)));
                    }
                }
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if status == 429 || status >= 500 {
                        let ra = parse_retry_after(
                            resp.headers()
                                .get("retry-after")
                                .and_then(|v| v.to_str().ok()),
                        );
                        let delay = backoff_delay(attempt, ra);
                        let delay = if ra.is_some() { delay } else { jittered(delay) };
                        last = Some(FetchError::Status {
                            status,
                            body: read_error_body(resp),
                        });
                        drop(pass);
                        if let (Some(gate), Some(_)) = (&self.gate, ra) {
                            // The server asked for a wait: every thread
                            // sharing the client waits, not only this one.
                            gate.pause(delay);
                        }
                        if attempt < self.retries {
                            std::thread::sleep(delay);
                        }
                    } else if !(200..300).contains(&status) {
                        return Err(FetchError::Status {
                            status,
                            body: read_error_body(resp),
                        });
                    } else {
                        return success(resp);
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| FetchError::Transport("no attempts made".to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httptest::{Expectation, Server, matchers::*, responders::*};

    fn client(retries: usize) -> FhirClient {
        FhirClient::new(Auth::None, retries, std::time::Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn backoff_doubles_from_half_a_second_and_is_clamped() {
        assert_eq!(backoff_delay(1, None).as_millis(), 500);
        assert_eq!(backoff_delay(2, None).as_millis(), 1000);
        assert_eq!(backoff_delay(3, None).as_millis(), 2000);
        assert_eq!(backoff_delay(20, None).as_secs(), 30);
        assert_eq!(backoff_delay(1, Some(7.0)).as_secs(), 7);
        assert_eq!(backoff_delay(1, Some(99999.0)).as_secs(), 30);
        assert_eq!(backoff_delay(1, Some(-5.0)).as_secs(), 0);
    }

    #[test]
    fn retry_after_parses_numeric_seconds_only() {
        assert_eq!(parse_retry_after(Some("7")), Some(7.0));
        assert_eq!(parse_retry_after(Some("1.5")), Some(1.5));
        assert_eq!(
            parse_retry_after(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            None
        );
        assert_eq!(parse_retry_after(None), None);
    }

    #[test]
    fn a_500_then_200_succeeds_with_retries() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/b"))
                .times(2)
                .respond_with(cycle![status_code(500), status_code(200).body("ok")]),
        );
        let got = client(3).get(&server.url("/b").to_string()).unwrap();
        assert_eq!(got.body, b"ok");
    }

    #[test]
    fn a_429_honours_retry_after() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/r"))
                .times(2)
                .respond_with(cycle![
                    status_code(429).append_header("Retry-After", "1"),
                    status_code(200).body("ok")
                ]),
        );
        let t = std::time::Instant::now();
        client(3).get(&server.url("/r").to_string()).unwrap();
        assert!(t.elapsed() >= std::time::Duration::from_secs(1));
    }

    #[test]
    fn a_404_is_not_retried() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/m"))
                .times(1)
                .respond_with(status_code(404)),
        );
        let err = client(3).get(&server.url("/m").to_string()).unwrap_err();
        assert!(
            matches!(err, FetchError::Status { status: 404, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn exhausted_retries_report_the_last_status() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/x"))
                .times(2)
                .respond_with(status_code(503)),
        );
        let err = client(2).get(&server.url("/x").to_string()).unwrap_err();
        assert!(matches!(err, FetchError::Status { status: 503, .. }));
    }

    #[test]
    fn connection_refused_is_a_transport_error() {
        let err = client(1).get("http://127.0.0.1:9/nothing").unwrap_err();
        assert!(matches!(err, FetchError::Transport(_)), "{err:?}");
    }

    #[test]
    fn bearer_and_accept_headers_are_sent() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/t"),
                request::headers(contains(("authorization", "Bearer secret"))),
                request::headers(contains(("accept", "application/fhir+json"))),
            ])
            .respond_with(status_code(200).body("ok")),
        );
        FhirClient::new(
            Auth::Token("secret".into()),
            1,
            std::time::Duration::from_secs(5),
        )
        .unwrap()
        .get(&server.url("/t").to_string())
        .unwrap();
    }

    fn credentials(server: &Server) -> Auth {
        Auth::ClientCredentials {
            token_url: server.url("/token").to_string(),
            client_id: "key-x".into(),
            client_secret: "s3cret&=".into(),
        }
    }

    fn token(access_token: &str, expires_in: u64) -> ResponseBuilder<String> {
        json_encoded(serde_json::json!({
            "access_token": access_token,
            "expires_in": expires_in,
            "token_type": "Bearer",
        }))
    }

    #[test]
    fn renewal_is_a_minute_early_or_at_half_life_for_short_tokens() {
        assert_eq!(renew_after(3600).as_secs(), 3540);
        assert_eq!(renew_after(121).as_secs(), 61);
        assert_eq!(renew_after(120).as_secs(), 60);
        assert_eq!(renew_after(1).as_secs(), 0);
    }

    #[test]
    fn client_credentials_are_traded_once_for_a_token_that_is_reused() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", "/token"),
                request::headers(contains((
                    "content-type",
                    "application/x-www-form-urlencoded"
                ))),
                request::headers(contains(("accept", "application/json"))),
                request::body(url_decoded(contains(("grant_type", "client_credentials")))),
                request::body(url_decoded(contains(("client_id", "key-x")))),
                request::body(url_decoded(contains(("client_secret", "s3cret&=")))),
            ])
            .times(1)
            .respond_with(token("abc", 3600)),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/t"),
                request::headers(contains(("authorization", "Bearer abc"))),
            ])
            .times(2)
            .respond_with(status_code(200).body("ok")),
        );
        let c = FhirClient::new(credentials(&server), 1, Duration::from_secs(5)).unwrap();
        c.get(&server.url("/t").to_string()).unwrap();
        c.get(&server.url("/t").to_string()).unwrap();
    }

    #[test]
    fn a_token_due_for_renewal_is_replaced_before_the_next_request() {
        let server = Server::run();
        // The first token lives one second, so it is due at once.
        server.expect(
            Expectation::matching(request::method_path("POST", "/token"))
                .times(2)
                .respond_with(cycle![token("old", 1), token("new", 3600)]),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/t"),
                request::headers(contains(("authorization", "Bearer new"))),
            ])
            .times(2)
            .respond_with(status_code(200).body("ok")),
        );
        let c = FhirClient::new(credentials(&server), 1, Duration::from_secs(5)).unwrap();
        c.get(&server.url("/t").to_string()).unwrap();
        c.get(&server.url("/t").to_string()).unwrap();
    }

    #[test]
    fn rejected_credentials_fail_before_any_fhir_request() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("POST", "/token"))
                .times(1)
                .respond_with(status_code(401).body(r#"{"error":"unauthorized_client"}"#)),
        );
        let Err(err) = FhirClient::new(credentials(&server), 3, Duration::from_secs(5)) else {
            panic!("expected the token request to fail");
        };
        let msg = err.to_string();
        assert!(msg.contains("cannot get a token from"), "{msg}");
        assert!(msg.contains("/token"), "{msg}");
        assert!(msg.contains("401"), "{msg}");
        assert!(msg.contains("unauthorized_client"), "{msg}");
    }

    #[test]
    fn a_token_response_without_an_access_token_is_an_error() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("POST", "/token"))
                .respond_with(json_encoded(serde_json::json!({"expires_in": 60}))),
        );
        let Err(err) = FhirClient::new(credentials(&server), 1, Duration::from_secs(5)) else {
            panic!("expected a missing access_token to fail");
        };
        assert!(err.to_string().contains("access_token"), "{err}");
    }

    fn google(json: &str) -> Auth {
        Auth::Google(Credentials::from_json(json, "test").unwrap())
    }

    fn metadata(server: &Server) -> Auth {
        let host = server.addr().to_string();
        Auth::Google(
            Credentials::discover(|k| match k {
                "GCE_METADATA_HOST" => Some(host.clone()),
                "CLOUDSDK_CONFIG" => Some("/nonexistent".into()),
                _ => None,
            })
            .unwrap(),
        )
    }

    const METADATA_PATH: &str = "/computeMetadata/v1/instance/service-accounts/default/token";

    #[test]
    fn a_service_account_signs_a_jwt_and_trades_it_for_a_token() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", "/token"),
                request::headers(contains((
                    "content-type",
                    "application/x-www-form-urlencoded"
                ))),
                request::body(url_decoded(contains((
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:jwt-bearer"
                )))),
                request::body(url_decoded(contains(key("assertion")))),
            ])
            .times(1)
            .respond_with(token("ya29.sa", 3600)),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Patient"),
                request::headers(contains(("authorization", "Bearer ya29.sa"))),
                request::headers(not(contains(key("x-goog-user-project")))),
            ])
            .times(2)
            .respond_with(status_code(200).body("ok")),
        );
        let json = google::tests::service_account_json(&server.url_str("/token"));
        let c = FhirClient::new(google(&json), 1, Duration::from_secs(5)).unwrap();
        c.get(&server.url_str("/fhir/Patient")).unwrap();
        c.get(&server.url_str("/fhir/Patient")).unwrap();
    }

    #[test]
    fn a_service_account_token_is_renewed_with_a_new_assertion() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", "/token"),
                request::body(url_decoded(contains(key("assertion")))),
            ])
            .times(2)
            .respond_with(cycle![token("old", 1), token("new", 3600)]),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/t"),
                request::headers(contains(("authorization", "Bearer new"))),
            ])
            .times(2)
            .respond_with(status_code(200).body("ok")),
        );
        let json = google::tests::service_account_json(&server.url_str("/token"));
        let c = FhirClient::new(google(&json), 1, Duration::from_secs(5)).unwrap();
        c.get(&server.url_str("/t")).unwrap();
        c.get(&server.url_str("/t")).unwrap();
    }

    #[test]
    fn a_rejected_service_account_fails_before_any_fhir_request() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("POST", "/token"))
                .times(1)
                .respond_with(status_code(400).body(
                    r#"{"error":"invalid_grant","error_description":"Invalid JWT Signature."}"#,
                )),
        );
        let json = google::tests::service_account_json(&server.url_str("/token"));
        let Err(err) = FhirClient::new(google(&json), 3, Duration::from_secs(5)) else {
            panic!("expected the token request to fail");
        };
        let msg = err.to_string();
        assert!(msg.contains(google::tests::TEST_EMAIL), "{msg}");
        assert!(msg.contains("invalid_grant"), "{msg}");
        assert!(!msg.contains("assertion"), "{msg}");
    }

    #[test]
    fn user_credentials_trade_their_refresh_token_and_name_the_quota_project() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", "/token"),
                request::body(url_decoded(contains(("grant_type", "refresh_token")))),
                request::body(url_decoded(contains(("refresh_token", "1//refresh-me")))),
                request::body(url_decoded(contains((
                    "client_id",
                    "user-client.apps.googleusercontent.com"
                )))),
                request::body(url_decoded(contains(("client_secret", "user-secret")))),
            ])
            .times(1)
            .respond_with(token("ya29.user", 3600)),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/t"),
                request::headers(contains(("authorization", "Bearer ya29.user"))),
                request::headers(contains(("x-goog-user-project", "billing-project"))),
            ])
            .times(1)
            .respond_with(status_code(200).body("ok")),
        );
        let json = google::tests::authorized_user_json(Some(&server.url_str("/token")));
        let c = FhirClient::new(google(&json), 1, Duration::from_secs(5)).unwrap();
        c.get(&server.url_str("/t")).unwrap();
    }

    #[test]
    fn the_metadata_server_gives_tokens_that_are_renewed() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", METADATA_PATH),
                request::headers(contains(("metadata-flavor", "Google"))),
            ])
            .times(2)
            .respond_with(cycle![token("meta-old", 1), token("meta-new", 3600)]),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/t"),
                request::headers(contains(("authorization", "Bearer meta-new"))),
            ])
            .times(2)
            .respond_with(status_code(200).body("ok")),
        );
        let c = FhirClient::new(metadata(&server), 1, Duration::from_secs(5)).unwrap();
        c.get(&server.url_str("/t")).unwrap();
        c.get(&server.url_str("/t")).unwrap();
    }

    #[test]
    fn no_metadata_server_is_an_error_that_says_what_to_set() {
        let auth = Auth::Google(
            Credentials::discover(|k| match k {
                "GCE_METADATA_HOST" => Some("127.0.0.1:9".into()),
                "CLOUDSDK_CONFIG" => Some("/nonexistent".into()),
                _ => None,
            })
            .unwrap(),
        );
        let started = Instant::now();
        let Err(err) = FhirClient::new(auth, 1, Duration::from_secs(300)) else {
            panic!("expected no metadata server");
        };
        assert!(started.elapsed() < Duration::from_secs(10));
        let msg = err.to_string();
        assert!(msg.contains("metadata server at 127.0.0.1:9"), "{msg}");
        assert!(msg.contains("GOOGLE_APPLICATION_CREDENTIALS"), "{msg}");
    }

    #[test]
    fn a_body_larger_than_the_cap_is_truncated_in_the_error() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/big"))
                .respond_with(status_code(400).body("x".repeat(2000))),
        );
        let err = client(1).get(&server.url("/big").to_string()).unwrap_err();
        match err {
            FetchError::Status { body, .. } => assert_eq!(body.chars().count(), 500),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn retry_after_is_honoured_on_a_503() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/s"))
                .times(2)
                .respond_with(cycle![
                    status_code(503).append_header("Retry-After", "1"),
                    status_code(200).body("ok")
                ]),
        );
        let t = std::time::Instant::now();
        client(3).get(&server.url("/s").to_string()).unwrap();
        assert!(t.elapsed() >= std::time::Duration::from_secs(1));
    }

    #[test]
    fn an_http_date_retry_after_falls_back_to_backoff() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/d"))
                .times(2)
                .respond_with(cycle![
                    status_code(503).append_header("Retry-After", "Wed, 21 Oct 2026 07:28:00 GMT"),
                    status_code(200).body("ok")
                ]),
        );
        let t = std::time::Instant::now();
        client(3).get(&server.url("/d").to_string()).unwrap();
        let elapsed = t.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "{elapsed:?}"
        );
        assert!(elapsed <= std::time::Duration::from_secs(5), "{elapsed:?}");
    }

    #[test]
    fn errors_reflect_the_final_attempt() {
        // Both attempts fail to connect: the reported error is the transport error.
        let err = client(2).get("http://127.0.0.1:9/nothing").unwrap_err();
        assert!(matches!(err, FetchError::Transport(_)), "{err:?}");

        // Both attempts get a 503: the reported error is that status.
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/y"))
                .times(2)
                .respond_with(status_code(503)),
        );
        let err = client(2).get(&server.url("/y").to_string()).unwrap_err();
        assert!(
            matches!(err, FetchError::Status { status: 503, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn post_json_sends_the_body_with_the_fhir_content_type_and_retries() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", "/fhir"),
                request::headers(contains(("content-type", "application/fhir+json"))),
                request::body(json_decoded(eq(
                    serde_json::json!({"resourceType": "Bundle"})
                ))),
            ])
            .times(2)
            .respond_with(cycle![
                status_code(503),
                status_code(200).body(r#"{"ok":true}"#)
            ]),
        );
        let got = client(3)
            .post_json(
                &server.url("/fhir").to_string(),
                br#"{"resourceType":"Bundle"}"#.to_vec(),
            )
            .unwrap();
        assert_eq!(got.body, br#"{"ok":true}"#);
    }

    #[test]
    fn a_201_is_a_success_and_a_412_is_final() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("POST", "/created"))
                .respond_with(status_code(201).body("made")),
        );
        server.expect(
            Expectation::matching(request::method_path("POST", "/stale"))
                .times(1)
                .respond_with(status_code(412).body("version conflict")),
        );
        let c = client(3);
        assert_eq!(
            c.post_json(&server.url("/created").to_string(), b"{}".to_vec())
                .unwrap()
                .body,
            b"made"
        );
        let err = c
            .post_json(&server.url("/stale").to_string(), b"{}".to_vec())
            .unwrap_err();
        assert!(
            matches!(err, FetchError::Status { status: 412, ref body } if body == "version conflict"),
            "{err:?}"
        );
    }
}
