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
//!
//! The parts are in `auth`, `gate` (the limit on requests in flight),
//! `retry` and `body` (gzip and body limits); this module holds the client
//! and its kinds of request.

mod auth;
mod body;
mod gate;
pub mod google;
mod retry;

pub use auth::{Auth, TokenAuth};

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use reqwest::Method;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_TYPE, HeaderMap, HeaderValue,
};

use crate::error::{PipesError, Result};

use auth::Authorizer;
pub use body::DEFAULT_BODY_LIMIT;
use body::{decoded, exchanged, fetched};
use gate::Gate;
use retry::{Attempt, server_wait};

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

    /// An unauthenticated client for one-off downloads (`--profile`, FHIR
    /// packages, rasters), shared by every caller asking for the same
    /// retries, timeout and body limit. Each reqwest blocking client starts a
    /// thread with a runtime of its own, so a run fetching several files
    /// starts one.
    pub fn shared(retries: usize, timeout: Duration, body_limit: u64) -> Result<Arc<Self>> {
        type Key = (usize, Duration, u64);
        static CLIENTS: Mutex<Vec<(Key, Arc<FhirClient>)>> = Mutex::new(Vec::new());
        let key = (retries, timeout, body_limit);
        let mut clients = CLIENTS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, c)) = clients.iter().find(|(k, _)| *k == key) {
            return Ok(Arc::clone(c));
        }
        let c = Arc::new(Self::new(Auth::None, retries, timeout)?.with_body_limit(body_limit));
        clients.push((key, Arc::clone(&c)));
        Ok(c)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::body::tests::{gzip, gzip_members};
    use httptest::{Expectation, Server, matchers::*, responders::*};

    pub(super) fn client(retries: usize) -> FhirClient {
        FhirClient::new(Auth::None, retries, std::time::Duration::from_secs(5)).unwrap()
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
