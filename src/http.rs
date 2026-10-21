//! The one HTTP client. Bearer auth, FHIR accept header, and the retry
//! policy shared by paging, boundary fetches and load's bundle posts:
//! connection errors, 5xx and 429 are retried with exponential backoff
//! (0.5 s doubling, clamped to [0, 30] s, a Retry-After replacing the
//! computed delay); any other status is final. With a limit
//! (`with_concurrency`, for `export --concurrency`) the threads sharing a
//! client keep at most that many requests in flight, and a Retry-After pauses
//! them all. The Authorization header is chosen for each attempt once it is
//! admitted, so a request that waited is not sent with an expired token.
//!
//! The bearer token is either fixed (`--token`), obtained with the OAuth
//! client-credentials grant (`--client-id`), or obtained from Google
//! Application Default Credentials (`--google-auth`, see `http/google.rs`). In the
//! last two cases the client gets a new token shortly before the current one
//! expires, so a run can outlast any one token. The client id and secret go
//! in the token request's form body (`client_secret_post`) or, with
//! `--token-auth basic`, in a Basic Authorization header
//! (`client_secret_basic`, which Aidbox expects).
//!
//! A 429 without a Retry-After is waited out for the `t=` of its `RateLimit`
//! header (draft-ietf-httpapi-ratelimit-headers), which Medplum sends
//! instead: the seconds until the quota is restored.
//!
//! Every request asks for gzip (`Accept-Encoding: gzip`) and callers see
//! the decompressed body, streamed downloads included. The client decodes
//! gzip itself, with `MultiGzDecoder`, which reads every gzip member of a
//! body; reqwest's own decoder stops after the first. FHIR GETs (searches,
//! counts, the Bulk Data kick-off and status polls) also send
//! `Cache-Control: no-cache`, so a server or proxy does not answer from a
//! cached search: HAPI otherwise reuses a recent search's results and total.
//!
//! `fhir-pipes dhis2` uses the same client against the DHIS2 Web API, with a
//! complete Authorization value (`ApiToken …` or `Basic …`) and plain JSON.
//!
//! `export --bulk` uses it for the Bulk Data kick-off, the status URL (whose
//! 202 and 429 the caller reads itself) and the output files.

pub mod google;

use std::io::Read;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use reqwest::Method;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE,
    HeaderMap, HeaderValue, RETRY_AFTER,
};
use serde_json::Value;

use self::google::Credentials;
use crate::error::{PipesError, Result};

const RETRY_BASE: f64 = 0.5;
const RETRY_MAX: f64 = 30.0;
/// The longest wait read from a Retry-After. Far more than a server asks
/// for in practice; it keeps a huge value from overflowing a `Duration` or
/// an `Instant`.
const RETRY_AFTER_MAX: Duration = Duration::from_secs(24 * 60 * 60);
const ERROR_BODY_CAP: usize = 500;
/// Never read more than this much of an error body off the wire; we only
/// keep the first ERROR_BODY_CAP chars of it anyway.
const ERROR_BODY_READ_LIMIT: u64 = 8192;
/// How much of a non-2xx body `exchange` keeps whole, for a caller that
/// parses it (a Bulk Data OperationOutcome). Beyond it the body is cut and
/// `truncated` set.
const EXCHANGE_BODY_LIMIT: u64 = 1 << 20;
/// The most a 2xx body read whole may decode to: far more than a search
/// page, a boundary or a package holds.
const DEFAULT_BODY_LIMIT: u64 = 1 << 30;
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
        /// How the id and secret are sent to `token_url`.
        token_auth: TokenAuth,
        /// The `scope` asked for, when given.
        scope: Option<String>,
    },
    /// Google credentials, usually `Credentials::application_default()`.
    Google(Credentials),
}

/// How a client-credentials token request carries the client id and secret
/// (OAuth 2.0, RFC 6749 section 2.3.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenAuth {
    /// In the form body: `client_secret_post`.
    #[default]
    Post,
    /// In a Basic Authorization header: `client_secret_basic`.
    Basic,
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
    /// `authorization` is the Basic header of `client_secret_basic`.
    Form {
        url: String,
        form: String,
        authorization: Option<HeaderValue>,
    },
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

/// The Basic header of `client_secret_basic`: the id and secret are each
/// form-urlencoded before they are joined and base64-encoded (RFC 6749
/// section 2.3.1). Ids and secrets of letters, digits and `*-._` are sent
/// as they are.
fn client_basic(client_id: &str, client_secret: &str) -> Result<HeaderValue> {
    use base64::Engine;
    let encode = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let pair = format!("{}:{}", encode(client_id), encode(client_secret));
    let mut v = HeaderValue::from_str(&format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    ))
    .map_err(|_| PipesError::Usage("the client credentials are not valid in a header".into()))?;
    v.set_sensitive(true);
    Ok(v)
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

    /// The wait the response asks for, read by `server_wait`.
    pub fn retry_after(&self) -> Option<Duration> {
        server_wait(self.status, &self.headers)
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

/// Whether `host` is this machine, where plain http does not cross a network.
fn is_this_machine(host: &str) -> bool {
    host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// A response's body as it was sent, before `Content-Encoding: gzip`.
/// `MultiGzDecoder` reads every member: concatenated gzip members are one
/// valid gzip body (RFC 1952 section 2.2), and a decoder that stops after
/// the first loses the rest without an error. Any other encoding is passed
/// through as it came; the client only asks for gzip. An empty body is
/// empty whatever its `Content-Encoding` says (a 202 from a server that
/// marks every response gzip), as reqwest's decoder treated it.
fn decoded(resp: Response) -> Box<dyn Read> {
    use std::io::BufRead as _;
    let gzip = resp
        .headers()
        .get(CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            let v = v.trim();
            v.eq_ignore_ascii_case("gzip") || v.eq_ignore_ascii_case("x-gzip")
        });
    if !gzip {
        return Box::new(resp);
    }
    let mut body = std::io::BufReader::new(resp);
    // After a read error here the decoder fails too: the same error again,
    // or a truncated stream.
    if body.fill_buf().is_ok_and(|b| b.is_empty()) {
        return Box::new(std::io::empty());
    }
    Box::new(flate2::bufread::MultiGzDecoder::new(body))
}

/// The whole decoded body, failing past `limit` bytes: a small gzip body can
/// decode to far more than memory holds.
fn read_body(resp: Response, limit: u64) -> std::result::Result<Vec<u8>, FetchError> {
    let mut body = Vec::new();
    decoded(resp)
        .take(limit.saturating_add(1))
        .read_to_end(&mut body)
        .map_err(|e| FetchError::Transport(format!("reading body: {e}")))?;
    if body.len() as u64 > limit {
        return Err(FetchError::Transport(format!(
            "the response is larger than {} MB once decoded; not read",
            limit >> 20
        )));
    }
    Ok(body)
}

/// A 2xx response's body and `Date`.
fn fetched(resp: Response, limit: u64) -> std::result::Result<Fetched, FetchError> {
    let date = resp
        .headers()
        .get(reqwest::header::DATE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Ok(Fetched {
        body: read_body(resp, limit)?,
        date,
    })
}

/// Any response, with its status and headers.
fn exchanged(resp: Response, limit: u64) -> std::result::Result<Exchange, FetchError> {
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let mut truncated = false;
    let body = if (200..300).contains(&status) {
        read_body(resp, limit)?
    } else {
        let mut buf = Vec::new();
        let _ = decoded(resp)
            .take(EXCHANGE_BODY_LIMIT + 1)
            .read_to_end(&mut buf);
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
    /// The origin (scheme, host and port) the credentials may go to, set by
    /// `for_server`. Requests to any other origin go without them.
    origin: Option<url::Origin>,
    /// The most a body read whole may decode to.
    body_limit: u64,
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

/// What `retrying` does before each attempt.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Attempt {
    /// Waits for a place in the gate, then adds the Authorization header as
    /// it is then, renewing a token that is due, when the request goes to
    /// the server's origin.
    Authorized,
    /// `Authorized`, to any origin: a Bulk Data output file whose manifest
    /// says it needs the token, which the spec lets live on another host.
    AuthorizedAnywhere,
    /// Waits for a place; no credentials (a signed storage URL).
    Anonymous,
    /// Neither: a token request. It runs while a request holds a place and
    /// the token lock, so it must not wait for another place.
    Token,
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

    /// Wait for a pause to end and a place to be free, giving up when
    /// `deadline` passes first.
    fn enter_by(&self, deadline: Option<Instant>) -> Option<Pass<'_>> {
        let mut state = self.lock();
        loop {
            let now = Instant::now();
            if deadline.is_some_and(|d| d <= now) {
                return None;
            }
            let wait = |until: Option<Instant>| {
                // The earlier of the pause's end and the deadline; neither
                // is a wait for a place.
                match (until, deadline) {
                    (Some(u), Some(d)) => Some(u.min(d) - now),
                    (u, d) => u.or(d).map(|t| t - now),
                }
            };
            let paused = state.paused_until.filter(|u| *u > now);
            if paused.is_none() && state.in_flight < state.limit {
                state.in_flight += 1;
                return Some(Pass(self));
            }
            state = match wait(paused) {
                Some(w) => {
                    self.changed
                        .wait_timeout(state, w)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
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

/// The wait before retry `attempt + 1`: the server's Retry-After, or 0.5 s
/// doubling with each attempt; either way at most RETRY_MAX.
pub fn backoff_delay(attempt: usize, retry_after: Option<Duration>) -> Duration {
    let max = Duration::from_secs_f64(RETRY_MAX);
    retry_after.map_or_else(
        || {
            let secs = RETRY_BASE * 2f64.powi(attempt.saturating_sub(1) as i32);
            Duration::from_secs_f64(secs.clamp(0.0, RETRY_MAX))
        },
        |wait| wait.min(max),
    )
}

/// A Retry-After value as a wait: delay-seconds or an HTTP-date (RFC 9110
/// section 10.2.3), clamped to [0, RETRY_AFTER_MAX]. Fractional seconds are
/// accepted, a negative number or a date in the past is no wait, and NaN or
/// anything else unreadable is None. The one parser for every caller, so no
/// server value can panic a `Duration` conversion.
pub fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let value = value?.trim();
    if let Ok(secs) = value.parse::<f64>() {
        if secs.is_nan() {
            return None;
        }
        let secs = secs.clamp(0.0, RETRY_AFTER_MAX.as_secs_f64());
        return Some(Duration::from_secs_f64(secs));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    let wait = at.duration_since(SystemTime::now()).unwrap_or_default();
    Some(wait.min(RETRY_AFTER_MAX))
}

/// The wait a response asks for before the next request: its Retry-After,
/// read by `parse_retry_after`, else, on a 429, the `t=` of its `RateLimit`
/// header, read by `parse_ratelimit_reset`. On any other status a
/// `RateLimit` header only reports the quota, so it is not a wait.
pub fn server_wait(status: u16, headers: &HeaderMap) -> Option<Duration> {
    let text = |name| headers.get(name).and_then(|v| v.to_str().ok());
    parse_retry_after(text(RETRY_AFTER)).or_else(|| {
        if status != 429 {
            return None;
        }
        // Several fields are one list (RFC 9110 section 5.3).
        let fields: Vec<&str> = headers
            .get_all("ratelimit")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        parse_ratelimit_reset((!fields.is_empty()).then(|| fields.join(", ")).as_deref())
    })
}

/// A `RateLimit` header's wait: the `t=` (seconds until the quota is
/// restored) of the policy that has run out, `r=0`, or the longest `t=`
/// when none says so. Medplum sends `"requests";r=0;t=60`. Clamped to
/// [0, RETRY_AFTER_MAX] as a Retry-After is; None when no item has a
/// readable `t=`.
pub fn parse_ratelimit_reset(value: Option<&str>) -> Option<Duration> {
    let mut spent = None;
    let mut any = None;
    for item in value?.split(',') {
        let mut remaining = None;
        let mut reset = None;
        for param in item.split(';').skip(1) {
            let Some((key, v)) = param.split_once('=') else {
                continue;
            };
            let v = v.trim().trim_matches('"');
            match key.trim() {
                "r" => remaining = v.parse::<f64>().ok(),
                "t" => reset = v.parse::<f64>().ok().filter(|t| !t.is_nan()),
                _ => {}
            }
        }
        let Some(t) = reset else {
            continue;
        };
        let wait = Duration::from_secs_f64(t.clamp(0.0, RETRY_AFTER_MAX.as_secs_f64()));
        any = any.max(Some(wait));
        if remaining.is_some_and(|r| r <= 0.0) {
            spent = spent.max(Some(wait));
        }
    }
    spent.or(any)
}

fn cap(body: String) -> String {
    body.chars().take(ERROR_BODY_CAP).collect()
}

/// Read at most ERROR_BODY_READ_LIMIT bytes of an error response, then cap
/// to ERROR_BODY_CAP chars. Bounded so a server that streams gigabytes of
/// "error" doesn't make us buffer it all just to report a status code.
fn read_error_body(resp: Response) -> String {
    let mut buf = Vec::new();
    let _ = decoded(resp)
        .take(ERROR_BODY_READ_LIMIT)
        .read_to_end(&mut buf);
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

    /// A different seed in every thread: `RandomState` keys come from the
    /// OS's random source, once per thread, and the thread id and the clock
    /// are hashed in as well.
    fn seed() -> u64 {
        use std::hash::BuildHasher as _;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        std::collections::hash_map::RandomState::new()
            .hash_one((std::thread::current().id(), nanos))
            | 1
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
        // Decoded by `decoded`, not by reqwest: reqwest's decoder reads only
        // the first gzip member. Its decoders are turned off here, not only
        // left out of Cargo.toml, because another crate in the build can
        // enable them (octofhir-fhir-model does, with `fhir-packages`).
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        let http = Client::builder()
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
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
            origin: None,
            body_limit: DEFAULT_BODY_LIMIT,
        };
        client.auth = client.authorizer(auth)?;
        Ok(client)
    }

    /// At most `limit` requests in flight at once from all the threads using
    /// this client, and a Retry-After on any response pauses them all.
    pub fn with_concurrency(mut self, limit: usize) -> Self {
        self.gate = Some(Gate::new(limit));
        self
    }

    /// Read whole bodies of up to `bytes` once decoded, in place of 1 GiB:
    /// for a raster, which can be larger.
    pub fn with_body_limit(mut self, bytes: u64) -> Self {
        self.body_limit = bytes;
        self
    }

    /// Send the credentials only to `server`'s origin (scheme, host and
    /// port). A URL taken from resource data (a boundary) or from a response
    /// (a next link, a status URL) can name another host, which must not get
    /// the server's token. Without this, no request gets the credentials.
    pub fn for_server(mut self, server: &str) -> Self {
        let url = url::Url::parse(server).ok();
        if let Some(u) = &url
            && u.scheme() == "http"
            && !matches!(self.auth, Authorizer::None)
            && !u.host_str().is_some_and(is_this_machine)
        {
            eprintln!("warning: {server} is plain http; the credentials go to it unencrypted");
        }
        self.origin = url.map(|u| u.origin());
        self
    }

    /// GET with the retry policy. Sleeps between attempts.
    pub fn get(&self, url: &str) -> std::result::Result<Fetched, FetchError> {
        self.send(|| self.fhir_get(url), |r| fetched(r, self.body_limit))
    }

    /// A FHIR GET: `no-cache`, so a search or count is not answered from a
    /// cached search (HAPI reuses one by default) or a proxy's copy.
    fn fhir_get(&self, url: &str) -> RequestBuilder {
        self.http.get(url).header(CACHE_CONTROL, "no-cache")
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
            |r| fetched(r, self.body_limit),
        )
    }

    /// POST a FHIR JSON body with the same retry policy. The body is cloned
    /// per attempt; bundles are small next to a boundary fetch.
    pub fn post_json(&self, url: &str, body: Vec<u8>) -> std::result::Result<Fetched, FetchError> {
        self.post_json_counted(url, body).0
    }

    /// `post_json`, with how many times the body was sent. After a timeout
    /// or a 5xx the server may have committed an earlier attempt, so the
    /// final answer can be about the server's own copy of this body.
    pub fn post_json_counted(
        &self,
        url: &str,
        body: Vec<u8>,
    ) -> (std::result::Result<Fetched, FetchError>, usize) {
        let sent = std::cell::Cell::new(0usize);
        let got = self.send(
            || {
                sent.set(sent.get() + 1);
                self.http
                    .post(url)
                    .header(CONTENT_TYPE, "application/fhir+json")
                    .body(body.clone())
            },
            |r| fetched(r, self.body_limit),
        );
        (got, sent.get())
    }

    /// GET with `Prefer: respond-async`, the Bulk Data kick-off. Retried as
    /// `get` is, except a 429, which comes back as an `Exchange` like the
    /// 202 (with its headers, for `Content-Location`): a server that runs
    /// one export at a time (Aidbox) answers 429 until the other one ends,
    /// and the caller waits that out against its own deadline. Every
    /// attempt and the waits between them end by `deadline`, as in
    /// `exchange_by`; no attempt starts after it.
    pub fn get_async(
        &self,
        url: &str,
        deadline: Option<Instant>,
    ) -> std::result::Result<Exchange, FetchError> {
        self.retrying_by(
            Attempt::Authorized,
            || self.fhir_get(url).header("prefer", "respond-async"),
            |status| status == 429,
            deadline,
            |r| exchanged(r, self.body_limit),
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
    /// when less time is left, and no retry starts after the deadline. A GET
    /// (a Bulk Data status poll) sends `no-cache`. A Retry-After on the
    /// response is the caller's to read, with `Exchange::retry_after`.
    pub fn exchange_by(
        &self,
        method: Method,
        url: &str,
        deadline: Option<Instant>,
    ) -> std::result::Result<Exchange, FetchError> {
        let request = || {
            if method == Method::GET {
                self.fhir_get(url)
            } else {
                self.http.request(method.clone(), url)
            }
        };
        // Every status is the caller's: only connection errors are retried.
        self.retrying_by(
            Attempt::Authorized,
            request,
            |_| true,
            deadline,
            |r| exchanged(r, self.body_limit),
        )
    }

    /// GET an NDJSON file and hand its body to `read` as it arrives, with the
    /// retry policy. A failure while reading is not retried, since `read` may
    /// have written part of the file. The Authorization header is sent only
    /// when `authorize`: a Bulk Data output kept elsewhere (a signed storage
    /// URL) must not get the server's token. `timeout` replaces the client's,
    /// for a file that takes longer than a search page should. A gzipped
    /// file reaches `read` decompressed. No `no-cache`: an export's files do
    /// not change, and a signed storage URL is best sent as it is.
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
        let body = |resp: Response| {
            read(&mut decoded(resp))
                .map_err(|e| FetchError::Transport(format!("reading body: {e}")))
        };
        let attempt = if authorize {
            Attempt::AuthorizedAnywhere
        } else {
            Attempt::Anonymous
        };
        self.retrying(attempt, request, body)
    }
}

impl FhirClient {
    /// The authorizer for `auth`. With client credentials or Google
    /// credentials the first token is fetched now.
    fn authorizer(&mut self, auth: Auth) -> Result<Authorizer> {
        Ok(match auth {
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
                token_auth,
                scope,
            } => {
                let mut form = url::form_urlencoded::Serializer::new(String::new());
                form.append_pair("grant_type", "client_credentials");
                if let Some(scope) = &scope {
                    form.append_pair("scope", scope);
                }
                let authorization = match token_auth {
                    TokenAuth::Post => {
                        form.append_pair("client_id", &client_id)
                            .append_pair("client_secret", &client_secret);
                        None
                    }
                    TokenAuth::Basic => Some(client_basic(&client_id, &client_secret)?),
                };
                self.renewing(Grant::Form {
                    url: token_url,
                    form: form.finish(),
                    authorization,
                })?
            }
            Auth::Google(creds) => {
                if let Some(project) = &creds.quota_project {
                    self.user_project = Some(HeaderValue::from_str(project).map_err(|_| {
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
                        authorization: None,
                    },
                    google::Source::Metadata { url } => Grant::Metadata { url: url.clone() },
                };
                self.renewing(grant).map_err(|e| {
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
        })
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

    /// One token request, with the retry policy. Returns the Authorization
    /// header and when to replace it, counted from before the request so the
    /// margin also covers the round trip.
    fn request_token(
        &self,
        grant: &Grant,
    ) -> std::result::Result<(HeaderValue, Instant), FetchError> {
        let started = Instant::now();
        let post = |url: &str, form: String, authorization: Option<&HeaderValue>| {
            self.retrying(
                Attempt::Token,
                || {
                    let request = self
                        .http
                        .post(url)
                        .header(ACCEPT, "application/json")
                        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(form.clone());
                    match authorization {
                        Some(v) => request.header(AUTHORIZATION, v.clone()),
                        None => request,
                    }
                },
                |r| fetched(r, self.body_limit),
            )
        };
        let got = match grant {
            Grant::Form {
                url,
                form,
                authorization,
            } => post(url, form.clone(), authorization.as_ref())?,
            Grant::Jwt(sa) => post(
                &sa.token_uri,
                sa.token_form().map_err(FetchError::Transport)?,
                None,
            )?,
            Grant::Metadata { url } => self.retrying(
                Attempt::Token,
                || {
                    self.http
                        .get(url)
                        .header(ACCEPT, "application/json")
                        .header("metadata-flavor", "Google")
                        .timeout(METADATA_TIMEOUT)
                },
                |r| fetched(r, self.body_limit),
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

    /// The Authorization header, and `x-goog-user-project` when set, when
    /// `request` goes to the server's origin, or to any with `anywhere`.
    fn authorize(
        &self,
        request: RequestBuilder,
        auth: &HeaderValue,
        anywhere: bool,
    ) -> std::result::Result<RequestBuilder, FetchError> {
        let (client, built) = request.build_split();
        let mut built = built.map_err(|e| FetchError::Transport(e.to_string()))?;
        if anywhere || self.origin.as_ref() == Some(&built.url().origin()) {
            let headers = built.headers_mut();
            headers.insert(AUTHORIZATION, auth.clone());
            if let Some(p) = &self.user_project {
                headers.insert("x-goog-user-project", p.clone());
            }
        }
        Ok(RequestBuilder::from_parts(client, built))
    }
}

impl FhirClient {
    /// A place in the gate, when the client has one, or Err when `deadline`
    /// passes before one is free.
    fn enter_by(&self, deadline: Option<Instant>) -> std::result::Result<Option<Pass<'_>>, ()> {
        match &self.gate {
            None => Ok(None),
            Some(gate) => gate.enter_by(deadline).map(Some).ok_or(()),
        }
    }
}

impl FhirClient {
    /// The retry loop with the Authorization header added to every attempt.
    fn send<T>(
        &self,
        request: impl Fn() -> RequestBuilder,
        success: impl FnOnce(Response) -> std::result::Result<T, FetchError>,
    ) -> std::result::Result<T, FetchError> {
        self.retrying(Attempt::Authorized, request, success)
    }

    /// The retry loop. Any 2xx is a success, handed to `success`; 429 and
    /// 5xx retry with backoff (the wait the server asks for, by
    /// `server_wait`, replaces the computed delay); any other status is
    /// final on the first response.
    fn retrying<T>(
        &self,
        kind: Attempt,
        request: impl Fn() -> RequestBuilder,
        success: impl FnOnce(Response) -> std::result::Result<T, FetchError>,
    ) -> std::result::Result<T, FetchError> {
        self.retrying_by(kind, request, |_| false, None, success)
    }

    /// `retrying`, with the statuses for which `handled` is true handed to
    /// `success` as a 2xx is, not retried. With a `deadline`, each attempt
    /// keeps the client's timeout, cut short when less time is left, the
    /// waits between attempts end by it, and no attempt starts after it;
    /// the last failure is then the error.
    fn retrying_by<T>(
        &self,
        kind: Attempt,
        request: impl Fn() -> RequestBuilder,
        handled: impl Fn(u16) -> bool,
        deadline: Option<Instant>,
        success: impl FnOnce(Response) -> std::result::Result<T, FetchError>,
    ) -> std::result::Result<T, FetchError> {
        let left = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let sleep = |wait: Duration| std::thread::sleep(left().map_or(wait, |l| wait.min(l)));
        let mut last: Option<FetchError> = None;
        for attempt in 1..=self.retries {
            let pass = match kind {
                Attempt::Token => None,
                Attempt::Authorized | Attempt::AuthorizedAnywhere | Attempt::Anonymous => {
                    match self.enter_by(deadline) {
                        Ok(pass) => pass,
                        Err(()) => break,
                    }
                }
            };
            let mut request = request();
            // Chosen once admitted, for every attempt: the wait for a place
            // and the sleeps between attempts can outlast a token, and the
            // token is renewed here when it is due.
            let anywhere = kind == Attempt::AuthorizedAnywhere;
            if (kind == Attempt::Authorized || anywhere)
                && let Some(v) = self.authorization()?
            {
                request = self.authorize(request, &v, anywhere)?;
            }
            // After both: the wait for a place and a token renewal take from
            // the time left.
            if let Some(left) = left() {
                if left.is_zero() {
                    break;
                }
                request = request.timeout(self.timeout.min(left));
            }
            match request.send() {
                Err(e) => {
                    drop(pass);
                    last = Some(FetchError::Transport(e.to_string()));
                    if attempt < self.retries {
                        sleep(jittered(backoff_delay(attempt, None)));
                    }
                }
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if handled(status) {
                        return success(resp);
                    }
                    if status == 429 || status >= 500 {
                        let ra = server_wait(status, resp.headers());
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
                            sleep(delay);
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
        Err(last.unwrap_or_else(|| {
            let why = if deadline.is_some() {
                "the deadline passed"
            } else {
                "no attempts made"
            };
            FetchError::Transport(why.to_string())
        }))
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
    fn the_gate_lets_in_only_its_limit() {
        let gate = Gate::new(2);
        let a = gate.enter_by(None).unwrap();
        let _b = gate.enter_by(None).unwrap();
        let entered = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                let _c = gate.enter_by(None).unwrap();
                entered.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(100));
            assert!(!entered.load(std::sync::atomic::Ordering::SeqCst));
            drop(a);
        });
        assert!(entered.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn a_pause_holds_every_request_back() {
        let gate = Gate::new(4);
        gate.pause(Duration::from_millis(200));
        // A shorter pause does not cut the longer one short.
        gate.pause(Duration::from_millis(10));
        let started = Instant::now();
        let _pass = gate.enter_by(None).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(150));
    }

    #[test]
    fn a_wait_for_the_gate_gives_up_at_the_deadline() {
        let gate = Gate::new(1);
        gate.pause(Duration::from_secs(5));
        let started = Instant::now();
        assert!(
            gate.enter_by(Some(started + Duration::from_millis(200)))
                .is_none()
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        // No pause, no free place: the same.
        let gate = Gate::new(1);
        let _held = gate.enter_by(None).unwrap();
        let started = Instant::now();
        assert!(
            gate.enter_by(Some(started + Duration::from_millis(200)))
                .is_none()
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_retry_after_pauses_the_other_threads_sharing_the_client() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/slow"))
                .times(2)
                .respond_with(cycle![
                    status_code(429).insert_header("retry-after", "1"),
                    status_code(200),
                ]),
        );
        server.expect(
            Expectation::matching(request::method_path("GET", "/other"))
                .times(1)
                .respond_with(status_code(200)),
        );
        let client = client(1).with_concurrency(4);
        client.get(&server.url_str("/slow")).unwrap_err();
        // The 429's Retry-After holds back a request to another URL too.
        let started = Instant::now();
        client.get(&server.url_str("/other")).unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        client.get(&server.url_str("/slow")).unwrap();
    }

    #[test]
    fn a_request_queued_past_its_tokens_expiry_is_sent_with_a_new_token() {
        // Two-second tokens, renewed after one. A 429 with Retry-After: 3
        // pauses the client; the request queued behind the pause is
        // admitted after tok-1 has expired and must go out with tok-2. The
        // gate has one place, so the renewal, made while that request holds
        // it, must not wait for a place itself.
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("POST", "/token"))
                .times(2)
                .respond_with(cycle![
                    json_encoded(serde_json::json!({"access_token": "tok-1", "expires_in": 2})),
                    json_encoded(serde_json::json!({"access_token": "tok-2", "expires_in": 2})),
                ]),
        );
        server.expect(
            Expectation::matching(request::method_path("GET", "/busy"))
                .times(1)
                .respond_with(status_code(429).insert_header("retry-after", "3")),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Patient"),
                request::headers(contains(("authorization", "Bearer tok-1"))),
            ])
            .times(0..)
            .respond_with(status_code(401)),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Patient"),
                request::headers(contains(("authorization", "Bearer tok-2"))),
            ])
            .times(1)
            .respond_with(status_code(200)),
        );
        let client = FhirClient::new(
            Auth::ClientCredentials {
                token_url: server.url_str("/token"),
                client_id: "id".into(),
                client_secret: "secret".into(),
                token_auth: TokenAuth::Post,
                scope: None,
            },
            1,
            Duration::from_secs(5),
        )
        .unwrap()
        .for_server(&server.url_str("/"))
        .with_concurrency(1);
        client.get(&server.url_str("/busy")).unwrap_err();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| tx.send(client.get(&server.url_str("/fhir/Patient"))));
            let got = rx
                .recv_timeout(Duration::from_secs(20))
                .expect("the queued request finished, not stuck behind its own renewal");
            got.unwrap();
        });
    }

    #[test]
    fn a_token_renewal_that_uses_up_the_deadline_stops_the_exchange() {
        // The first token is due at once; renewing it takes 900 ms, more
        // than the exchange's 400 ms. The status URL must not be asked after
        // the deadline.
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("POST", "/token"))
                .times(2)
                .respond_with(cycle![
                    json_encoded(serde_json::json!({"access_token": "tok-1", "expires_in": 0})),
                    delay_and_then(
                        Duration::from_millis(900),
                        json_encoded(serde_json::json!({"access_token": "tok-2", "expires_in": 0})),
                    ),
                ]),
        );
        server.expect(
            Expectation::matching(request::method_path("GET", "/status"))
                .times(0)
                .respond_with(status_code(200)),
        );
        let client = FhirClient::new(
            Auth::ClientCredentials {
                token_url: server.url_str("/token"),
                client_id: "id".into(),
                client_secret: "secret".into(),
                token_auth: TokenAuth::Post,
                scope: None,
            },
            3,
            Duration::from_secs(5),
        )
        .unwrap()
        .for_server(&server.url_str("/"));
        let deadline = Instant::now() + Duration::from_millis(400);
        let err = client
            .exchange_by(Method::GET, &server.url_str("/status"), Some(deadline))
            .unwrap_err();
        assert!(err.to_string().contains("deadline"), "{err}");
    }

    #[test]
    fn backoff_doubles_from_half_a_second_and_is_clamped() {
        assert_eq!(backoff_delay(1, None).as_millis(), 500);
        assert_eq!(backoff_delay(2, None).as_millis(), 1000);
        assert_eq!(backoff_delay(3, None).as_millis(), 2000);
        assert_eq!(backoff_delay(20, None).as_secs(), 30);
        assert_eq!(backoff_delay(1, Some(Duration::from_secs(7))).as_secs(), 7);
        assert_eq!(backoff_delay(1, Some(RETRY_AFTER_MAX)).as_secs(), 30);
        assert_eq!(backoff_delay(1, Some(Duration::ZERO)).as_secs(), 0);
    }

    #[test]
    fn retry_after_reads_seconds_and_http_dates_and_clamps() {
        let secs = |v: &str| parse_retry_after(Some(v));
        assert_eq!(secs("7"), Some(Duration::from_secs(7)));
        assert_eq!(secs(" 1.5 "), Some(Duration::from_millis(1500)));
        // Out of range: never a panic, clamped to [0, RETRY_AFTER_MAX].
        assert_eq!(secs("1e300"), Some(RETRY_AFTER_MAX));
        assert_eq!(secs("inf"), Some(RETRY_AFTER_MAX));
        assert_eq!(secs("-5"), Some(Duration::ZERO));
        assert_eq!(secs("-1e300"), Some(Duration::ZERO));
        assert_eq!(secs("-inf"), Some(Duration::ZERO));
        assert_eq!(secs("NaN"), None);
        assert_eq!(secs("soon"), None);
        assert_eq!(secs(""), None);
        assert_eq!(parse_retry_after(None), None);
        // An HTTP-date: the time until then, nothing for one in the past,
        // RETRY_AFTER_MAX for one far ahead.
        assert_eq!(secs("Sun, 06 Nov 1994 08:49:37 GMT"), Some(Duration::ZERO));
        assert_eq!(secs("Fri, 31 Dec 9999 23:59:59 GMT"), Some(RETRY_AFTER_MAX));
        let soon = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(120));
        let wait = secs(&soon).unwrap();
        assert!(
            wait > Duration::from_secs(110) && wait <= Duration::from_secs(120),
            "{wait:?}"
        );
    }

    #[test]
    fn a_ratelimit_header_gives_the_reset_of_the_spent_policy() {
        let reset = |v: &str| parse_ratelimit_reset(Some(v));
        assert_eq!(
            reset(r#""requests";r=0;t=60"#),
            Some(Duration::from_secs(60))
        );
        assert_eq!(reset("default;r=0;t=2"), Some(Duration::from_secs(2)));
        // The policy that has run out, not the longer wait of one that has not.
        assert_eq!(
            reset(r#""fhir";r=500;t=50, "requests";r=0;t=30"#),
            Some(Duration::from_secs(30))
        );
        // None has run out: the longest.
        assert_eq!(
            reset(r#""a";r=5;t=10, "b";r=3;t=40"#),
            Some(Duration::from_secs(40))
        );
        assert_eq!(reset(r#""a";r=0;t=1e300"#), Some(RETRY_AFTER_MAX));
        assert_eq!(reset(r#""a";r=0;t=-3"#), Some(Duration::ZERO));
        assert_eq!(reset(r#""a";r=0;t=NaN"#), None);
        // The older draft's form has no `t=`.
        assert_eq!(reset("limit=100, remaining=0, reset=60"), None);
        assert_eq!(reset(""), None);
        assert_eq!(parse_ratelimit_reset(None), None);
    }

    #[test]
    fn a_ratelimit_header_is_a_wait_only_on_a_429_without_a_retry_after() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.append(*k, HeaderValue::from_static(v));
            }
            h
        };
        let limited = headers(&[("ratelimit", "\"requests\";r=0;t=7")]);
        assert_eq!(server_wait(429, &limited), Some(Duration::from_secs(7)));
        assert_eq!(server_wait(202, &limited), None);
        assert_eq!(server_wait(503, &limited), None);
        let both = headers(&[("retry-after", "3"), ("ratelimit", "\"requests\";r=0;t=7")]);
        assert_eq!(server_wait(429, &both), Some(Duration::from_secs(3)));
        // Two RateLimit fields are read as one list.
        let two = headers(&[
            ("ratelimit", "\"fhir\";r=9;t=50"),
            ("ratelimit", "\"requests\";r=0;t=20"),
        ]);
        assert_eq!(server_wait(429, &two), Some(Duration::from_secs(20)));
    }

    #[test]
    fn medplums_two_ratelimit_fields_are_read_as_it_sends_them() {
        // Self-hosted Medplum 5.2.5 sends both fields on every response and
        // no Retry-After on a 429.
        let headers = |requests: &'static str, fhir: &'static str| {
            let mut h = HeaderMap::new();
            h.append("ratelimit", HeaderValue::from_static(requests));
            h.append("ratelimit", HeaderValue::from_static(fhir));
            h
        };
        let quota = headers(
            "\"requests\";r=59990;t=60",
            "\"fhirInteractions\";r=49980;t=60",
        );
        // Not a 429: the fields only report the quota.
        assert_eq!(server_wait(200, &quota), None);
        assert_eq!(server_wait(202, &quota), None);
        // A 429 with neither spent: the longest reset.
        assert_eq!(server_wait(429, &quota), Some(Duration::from_secs(60)));
        // One spent: its reset, not the other's.
        let spent = headers("\"requests\";r=59990;t=60", "\"fhirInteractions\";r=0;t=45");
        assert_eq!(server_wait(429, &spent), Some(Duration::from_secs(45)));
        let spent = headers("\"requests\";r=0;t=12", "\"fhirInteractions\";r=49980;t=60");
        assert_eq!(server_wait(429, &spent), Some(Duration::from_secs(12)));
    }

    #[test]
    fn each_thread_draws_its_own_jitter() {
        // Seeded from the clock's elapsed nanoseconds, every thread's first
        // factor was about 0, so retries after a shared 5xx stayed in step.
        let threads: Vec<_> = (0..8).map(|_| std::thread::spawn(jitter_factor)).collect();
        let mut firsts: Vec<f64> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(firsts.iter().all(|f| (0.0..1.0).contains(f)), "{firsts:?}");
        firsts.sort_by(f64::total_cmp);
        assert!(firsts[7] - firsts[0] > 0.01, "{firsts:?}");
        firsts.dedup();
        assert_eq!(firsts.len(), 8, "{firsts:?}");
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    /// `parts` as one gzip body of a member each (RFC 1952 section 2.2).
    fn gzip_members(parts: &[&[u8]]) -> Vec<u8> {
        parts.iter().flat_map(|p| gzip(p)).collect()
    }

    #[test]
    fn every_member_of_a_multi_member_gzip_page_is_read() {
        let page = br#"{"resourceType":"Bundle","type":"searchset","total":2}"#;
        let (a, b) = page.split_at(20);
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/Patient"))
                .times(1)
                .respond_with(
                    status_code(200)
                        .insert_header("content-encoding", "gzip")
                        .body(gzip_members(&[a, b])),
                ),
        );
        let got = client(1).get(&server.url_str("/fhir/Patient")).unwrap();
        assert_eq!(got.body, page);
    }

    #[test]
    fn a_server_that_ignores_accept_encoding_is_read_as_sent() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/plain"),
                request::headers(contains(("accept-encoding", matches("gzip")))),
            ])
            .times(1)
            .respond_with(status_code(200).body("plain text")),
        );
        let got = client(1).get(&server.url_str("/plain")).unwrap();
        assert_eq!(got.body, b"plain text");
    }

    #[test]
    fn an_empty_gzip_body_and_a_gzipped_error_are_decoded() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/empty"))
                .times(1)
                .respond_with(status_code(202).insert_header("content-encoding", "gzip")),
        );
        server.expect(
            Expectation::matching(request::method_path("GET", "/missing"))
                .times(1)
                .respond_with(
                    status_code(404)
                        .insert_header("content-encoding", "gzip")
                        .body(gzip_members(&[b"no such ", b"resource"])),
                ),
        );
        let c = client(1);
        let got = c.exchange(Method::GET, &server.url_str("/empty")).unwrap();
        assert_eq!((got.status, got.body.len()), (202, 0));
        let err = c.get(&server.url_str("/missing")).unwrap_err();
        assert!(
            matches!(err, FetchError::Status { status: 404, ref body } if body == "no such resource"),
            "{err:?}"
        );
    }

    #[test]
    fn a_corrupt_gzip_body_is_an_error() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/bad"))
                .times(1)
                .respond_with(
                    status_code(200)
                        .insert_header("content-encoding", "gzip")
                        .body("not gzip"),
                ),
        );
        let err = client(1).get(&server.url_str("/bad")).unwrap_err();
        assert!(matches!(err, FetchError::Transport(_)), "{err:?}");
    }

    #[test]
    fn a_search_asks_for_gzip_and_no_cache_and_gets_the_decompressed_body() {
        let page = br#"{"resourceType":"Bundle","type":"searchset","total":2}"#;
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Patient"),
                request::headers(contains(("accept-encoding", matches("gzip")))),
                request::headers(contains(("cache-control", "no-cache"))),
            ])
            .times(1)
            .respond_with(
                status_code(200)
                    .insert_header("content-encoding", "gzip")
                    .body(gzip(page)),
            ),
        );
        let got = client(1).get(&server.url_str("/fhir/Patient")).unwrap();
        assert_eq!(got.body, page);
    }

    #[test]
    fn a_bulk_kick_off_and_status_poll_send_no_cache_and_a_delete_does_not() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/$export"),
                request::headers(contains(("accept-encoding", matches("gzip")))),
                request::headers(contains(("cache-control", "no-cache"))),
                request::headers(contains(("prefer", "respond-async"))),
            ])
            .times(1)
            .respond_with(status_code(202).insert_header("content-location", "/status")),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/status"),
                request::headers(contains(("accept-encoding", matches("gzip")))),
                request::headers(contains(("cache-control", "no-cache"))),
            ])
            .times(1)
            .respond_with(
                status_code(200)
                    .insert_header("content-encoding", "gzip")
                    .body(gzip_members(&[br#"{"outp"#, br#"ut":[]}"#])),
            ),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("DELETE", "/status"),
                request::headers(not(contains(key("cache-control")))),
            ])
            .times(1)
            .respond_with(status_code(202)),
        );
        let c = client(1);
        assert_eq!(
            c.get_async(&server.url_str("/fhir/$export"), None)
                .unwrap()
                .status,
            202
        );
        let polled = c.exchange(Method::GET, &server.url_str("/status")).unwrap();
        assert_eq!(polled.body, br#"{"output":[]}"#);
        c.exchange(Method::DELETE, &server.url_str("/status"))
            .unwrap();
    }

    #[test]
    fn every_member_of_a_gzipped_download_reaches_the_reader() {
        let lines = "{\"resourceType\":\"Patient\",\"id\":\"a\"}\n".repeat(1000);
        let (head, tail) = lines.as_bytes().split_at(lines.len() / 2);
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/files/p.ndjson"),
                request::headers(contains(("accept", "application/fhir+ndjson"))),
                request::headers(contains(("accept-encoding", matches("gzip")))),
            ])
            .times(1)
            .respond_with(
                status_code(200)
                    .insert_header("content-encoding", "gzip")
                    .body(gzip_members(&[head, tail])),
            ),
        );
        let got = client(1)
            .download(
                &server.url_str("/files/p.ndjson"),
                false,
                Duration::from_secs(5),
                |body| {
                    let mut text = String::new();
                    body.read_to_string(&mut text)?;
                    Ok(text)
                },
            )
            .unwrap();
        assert_eq!(got, lines);
    }

    #[test]
    fn a_token_request_does_not_send_no_cache() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", "/token"),
                request::headers(not(contains(key("cache-control")))),
            ])
            .times(1)
            .respond_with(token("abc", 3600)),
        );
        FhirClient::new(credentials(&server), 1, Duration::from_secs(5)).unwrap();
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
    fn a_429_with_only_a_ratelimit_header_waits_for_its_reset() {
        // Medplum's form: no Retry-After. The backoff alone would retry
        // after half a second.
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/r"))
                .times(2)
                .respond_with(cycle![
                    status_code(429).append_header("RateLimit", "\"requests\";r=0;t=2"),
                    status_code(200).body("ok")
                ]),
        );
        let t = Instant::now();
        client(3).get(&server.url_str("/r")).unwrap();
        assert!(t.elapsed() >= Duration::from_secs(2), "{:?}", t.elapsed());
    }

    #[test]
    fn a_429_with_medplums_two_ratelimit_fields_waits_for_the_spent_one() {
        // The `requests` policy's 60 s would be capped at 30 s; the spent
        // `fhirInteractions` policy asks for 2 s.
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/r"))
                .times(2)
                .respond_with(cycle![
                    status_code(429)
                        .append_header("RateLimit", "\"requests\";r=59990;t=60")
                        .append_header("RateLimit", "\"fhirInteractions\";r=0;t=2"),
                    status_code(200).body("ok")
                ]),
        );
        let t = Instant::now();
        client(3).get(&server.url_str("/r")).unwrap();
        assert!(t.elapsed() >= Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    }

    #[test]
    fn a_bulk_kick_off_hands_a_429_back_without_retrying() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/$export"))
                .times(1)
                .respond_with(status_code(429).append_header("Retry-After", "5")),
        );
        let got = client(3)
            .get_async(&server.url_str("/fhir/$export"), None)
            .unwrap();
        assert_eq!(got.status, 429);
        assert_eq!(got.retry_after(), Some(Duration::from_secs(5)));
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
    fn a_body_that_decodes_past_the_limit_is_not_read() {
        let server = Server::run();
        // 2 MB of zeros, a few KB once gzipped.
        server.expect(
            Expectation::matching(request::method_path("GET", "/big")).respond_with(
                status_code(200)
                    .insert_header("content-encoding", "gzip")
                    .body(gzip_members(&[&vec![0u8; 2 << 20]])),
            ),
        );
        let err = client(1)
            .with_body_limit(1 << 20)
            .get(&server.url_str("/big"))
            .unwrap_err();
        assert!(err.to_string().contains("larger than 1 MB"), "{err}");
    }

    #[test]
    fn the_token_goes_only_to_the_servers_origin() {
        let fhir = Server::run();
        let elsewhere = Server::run();
        fhir.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Location"),
                request::headers(contains(("authorization", "Bearer secret"))),
            ])
            .respond_with(status_code(200)),
        );
        // A boundary URL from a Location, on another host (another port
        // is another origin): no token.
        elsewhere.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/boundary.geojson"),
                request::headers(not(contains(key("authorization")))),
            ])
            .respond_with(status_code(200)),
        );
        // A Bulk Data file whose manifest says it needs the token: sent.
        elsewhere.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/files/p.ndjson"),
                request::headers(contains(("authorization", "Bearer secret"))),
            ])
            .respond_with(status_code(200)),
        );
        let c = FhirClient::new(Auth::Token("secret".into()), 1, Duration::from_secs(5))
            .unwrap()
            .for_server(&fhir.url_str("/fhir"));
        c.get(&fhir.url_str("/fhir/Location")).unwrap();
        c.get(&elsewhere.url_str("/boundary.geojson")).unwrap();
        c.download(
            &elsewhere.url_str("/files/p.ndjson"),
            true,
            Duration::from_secs(5),
            |_| Ok(()),
        )
        .unwrap();
    }

    #[test]
    fn without_a_server_no_request_gets_the_token() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::headers(not(contains(key("authorization")))))
                .respond_with(status_code(200)),
        );
        FhirClient::new(Auth::Token("secret".into()), 1, Duration::from_secs(5))
            .unwrap()
            .get(&server.url_str("/t"))
            .unwrap();
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
        .for_server(&server.url_str("/"))
        .get(&server.url("/t").to_string())
        .unwrap();
    }

    fn credentials(server: &Server) -> Auth {
        Auth::ClientCredentials {
            token_url: server.url("/token").to_string(),
            client_id: "key-x".into(),
            client_secret: "s3cret&=".into(),
            token_auth: TokenAuth::Post,
            scope: None,
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
        let c = FhirClient::new(credentials(&server), 1, Duration::from_secs(5))
            .unwrap()
            .for_server(&server.url_str("/"));
        c.get(&server.url("/t").to_string()).unwrap();
        c.get(&server.url("/t").to_string()).unwrap();
    }

    #[test]
    fn token_auth_basic_sends_the_id_and_secret_only_in_a_basic_header() {
        // base64("key-x:s3cret%26%3D"): each part form-urlencoded first.
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", "/token"),
                request::headers(contains((
                    "authorization",
                    "Basic a2V5LXg6czNjcmV0JTI2JTNE"
                ))),
                request::body(url_decoded(contains(("grant_type", "client_credentials")))),
                request::body(url_decoded(not(contains(key("client_id"))))),
                request::body(url_decoded(not(contains(key("client_secret"))))),
                request::body(url_decoded(not(contains(key("scope"))))),
            ])
            .times(1)
            .respond_with(token("abc", 3600)),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/t"),
                request::headers(contains(("authorization", "Bearer abc"))),
            ])
            .times(1)
            .respond_with(status_code(200).body("ok")),
        );
        let Auth::ClientCredentials {
            token_url,
            client_id,
            client_secret,
            ..
        } = credentials(&server)
        else {
            unreachable!()
        };
        let auth = Auth::ClientCredentials {
            token_url,
            client_id,
            client_secret,
            token_auth: TokenAuth::Basic,
            scope: None,
        };
        let c = FhirClient::new(auth, 1, Duration::from_secs(5))
            .unwrap()
            .for_server(&server.url_str("/"));
        c.get(&server.url_str("/t")).unwrap();
    }

    #[test]
    fn a_scope_is_sent_with_the_token_request() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", "/token"),
                request::body(url_decoded(contains(("scope", "system/*.read")))),
                request::body(url_decoded(contains(("client_id", "key-x")))),
            ])
            .times(1)
            .respond_with(token("abc", 3600)),
        );
        let Auth::ClientCredentials {
            token_url,
            client_id,
            client_secret,
            token_auth,
            ..
        } = credentials(&server)
        else {
            unreachable!()
        };
        let auth = Auth::ClientCredentials {
            token_url,
            client_id,
            client_secret,
            token_auth,
            scope: Some("system/*.read".into()),
        };
        FhirClient::new(auth, 1, Duration::from_secs(5)).unwrap();
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
        let c = FhirClient::new(credentials(&server), 1, Duration::from_secs(5))
            .unwrap()
            .for_server(&server.url_str("/"));
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
        let c = FhirClient::new(google(&json), 1, Duration::from_secs(5))
            .unwrap()
            .for_server(&server.url_str("/"));
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
        let c = FhirClient::new(google(&json), 1, Duration::from_secs(5))
            .unwrap()
            .for_server(&server.url_str("/"));
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
        let c = FhirClient::new(google(&json), 1, Duration::from_secs(5))
            .unwrap()
            .for_server(&server.url_str("/"));
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
        let c = FhirClient::new(metadata(&server), 1, Duration::from_secs(5))
            .unwrap()
            .for_server(&server.url_str("/"));
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
    fn an_http_date_retry_after_is_honoured() {
        // Two seconds ahead, to whole seconds: a wait of more than one.
        let at = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(2));
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/d"))
                .times(2)
                .respond_with(cycle![
                    status_code(503).append_header("Retry-After", at.as_str()),
                    status_code(200).body("ok")
                ]),
        );
        let t = std::time::Instant::now();
        client(3).get(&server.url("/d").to_string()).unwrap();
        let elapsed = t.elapsed();
        assert!(elapsed >= Duration::from_millis(900), "{elapsed:?}");
        assert!(elapsed <= Duration::from_secs(5), "{elapsed:?}");
    }

    #[test]
    fn a_huge_retry_after_is_clamped_to_the_backoff_ceiling() {
        // One attempt, so the clamped wait is not slept; the point is that
        // the value does not panic the conversion or the gate's pause.
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/h"))
                .times(1)
                .respond_with(status_code(503).append_header("Retry-After", "1e300")),
        );
        let c = client(1).with_concurrency(2);
        let err = c.get(&server.url_str("/h")).unwrap_err();
        assert!(
            matches!(err, FetchError::Status { status: 503, .. }),
            "{err:?}"
        );
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
