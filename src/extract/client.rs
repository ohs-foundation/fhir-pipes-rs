//! The one HTTP client. Bearer auth, FHIR accept header, and the retry
//! policy shared by paging, boundary fetches and load's bundle posts:
//! connection errors, 5xx and 429 are retried with exponential backoff
//! (0.5 s doubling, clamped to [0, 30] s, a numeric Retry-After replacing
//! the computed delay); any other status is final.
//!
//! The bearer token is either fixed (`--token`) or obtained with the OAuth
//! client-credentials grant (`--client-id`), in which case the client gets
//! a new token shortly before the current one expires, so a run can outlast
//! any one token.
//!
//! `fhir-pipes dhis2` uses the same client against the DHIS2 Web API, with a
//! complete Authorization value (`ApiToken …` or `Basic …`) and plain JSON.
//!
//! `export --bulk` uses it for the Bulk Data kick-off, the status URL (whose
//! 202 and 429 the caller reads itself) and the output files.

use std::io::Read as _;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use reqwest::Method;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::Value;

use crate::error::{PipesError, Result};

const RETRY_BASE: f64 = 0.5;
const RETRY_MAX: f64 = 30.0;
const ERROR_BODY_CAP: usize = 500;
/// Never read more than this much of an error body off the wire; we only
/// keep the first ERROR_BODY_CAP chars of it anyway.
const ERROR_BODY_READ_LIMIT: u64 = 8192;
/// Assumed token lifetime, in seconds, when a token response omits `expires_in`.
const DEFAULT_TOKEN_LIFETIME: u64 = 300;

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
}

/// The current client-credentials token and when to replace it. The lock is
/// held across a renewal so concurrent boundary workers wait for one new
/// token instead of each fetching their own.
struct TokenSource {
    url: String,
    form: String,
    current: Mutex<(HeaderValue, Instant)>,
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
    /// The whole body of a 2xx; at most ERROR_BODY_CAP chars of any other.
    pub body: Vec<u8>,
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
    let body = if (200..300).contains(&status) {
        resp.bytes()
            .map_err(|e| FetchError::Transport(format!("reading body: {e}")))?
            .to_vec()
    } else {
        read_error_body(resp).into_bytes()
    };
    Ok(Exchange {
        status,
        headers,
        body,
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
    auth: Authorizer,
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
fn ensure_crypto_provider() {
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
    /// With client credentials the first token is fetched here, so a wrong
    /// secret or token URL fails before any FHIR request is made.
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
            auth: Authorizer::None,
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
                let first = client.request_token(&token_url, &form).map_err(|e| {
                    PipesError::Environment(format!("cannot get a token from {token_url}: {e}"))
                })?;
                Authorizer::Renewing(TokenSource {
                    url: token_url,
                    form,
                    current: Mutex::new(first),
                })
            }
        };
        Ok(client)
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
        let auth = self.authorization()?;
        let mut last = None;
        for attempt in 1..=self.retries {
            let mut request = self.http.request(method.clone(), url);
            if let Some(v) = &auth {
                request = request.header(AUTHORIZATION, v.clone());
            }
            match request.send() {
                Ok(resp) => return exchanged(resp),
                Err(e) => {
                    last = Some(FetchError::Transport(e.to_string()));
                    if attempt < self.retries {
                        std::thread::sleep(jittered(backoff_delay(attempt, None)));
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| FetchError::Transport("no attempts made".to_string())))
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

    /// One client-credentials token request, with the retry policy. Returns
    /// the Authorization header and when to replace it, counted from before
    /// the request so the margin also covers the round trip.
    fn request_token(
        &self,
        url: &str,
        form: &str,
    ) -> std::result::Result<(HeaderValue, Instant), FetchError> {
        let started = Instant::now();
        let got = self.retrying(
            || {
                self.http
                    .post(url)
                    .header(ACCEPT, "application/json")
                    .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(form.to_owned())
            },
            fetched,
        )?;
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
                    *current = self.request_token(&src.url, &src.form).map_err(|e| {
                        FetchError::Transport(format!("renewing the token at {}: {e}", src.url))
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
                Some(v) => request().header(AUTHORIZATION, v.clone()),
                None => request(),
            },
            success,
        )
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
            match request().send() {
                Err(e) => {
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
