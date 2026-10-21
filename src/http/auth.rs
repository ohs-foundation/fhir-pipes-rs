//! How requests are authorised: a fixed header, or a token from the OAuth
//! client-credentials grant or Google credentials, renewed shortly before
//! it expires, and sent only to the server's origin.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use reqwest::blocking::RequestBuilder;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde_json::Value;

use crate::error::{PipesError, Result};

use super::body::fetched;
use super::google::{self, Credentials};
use super::retry::Attempt;
use super::{FetchError, FhirClient};

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
pub(super) struct TokenSource {
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

pub(super) enum Authorizer {
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

impl FhirClient {
    /// The authorizer for `auth`. With client credentials or Google
    /// credentials the first token is fetched now.
    pub(super) fn authorizer(&mut self, auth: Auth) -> Result<Authorizer> {
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
    pub(super) fn authorization(&self) -> std::result::Result<Option<HeaderValue>, FetchError> {
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
    pub(super) fn authorize(
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

#[cfg(test)]
mod tests {
    use super::*;
    use httptest::{Expectation, Server, matchers::*, responders::*};
    use reqwest::Method;

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
}
