//! `[serve.auth]` in a running `fhir-pipes serve`: access tokens from an
//! identity provider (an httptest server publishing its OpenID configuration
//! and keys), which paths each token opens, the 401 and 403 answers, the
//! rules for a non-loopback `--listen`, and what reaches the log.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use httptest::matchers::request;
use httptest::responders::json_encoded;
use httptest::{Expectation, Server};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::json;

use common::serve::*;
use common::*;

const AUDIENCE: &str = "fhir-pipes";
const QUERY_SCOPE: &str = "fhir-pipes.query";
const ADMIN_SCOPE: &str = "fhir-pipes.admin";

/// An identity provider: its signing key and the server publishing it.
struct Provider {
    server: Server,
    key: EcdsaKeyPair,
}

impl Provider {
    fn start() -> Provider {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let p = key.public_key().as_ref();
        let jwk = json!({"kty": "EC", "crv": "P-256", "kid": "k1", "use": "sig",
                         "x": URL_SAFE_NO_PAD.encode(&p[1..33]), "y": URL_SAFE_NO_PAD.encode(&p[33..65])});
        let server = Server::run();
        let issuer = server.url_str("/realms/test");
        server.expect(
            Expectation::matching(request::path(
                "/realms/test/.well-known/openid-configuration",
            ))
            .times(..)
            .respond_with(json_encoded(json!({
                "issuer": issuer,
                "jwks_uri": server.url_str("/realms/test/certs"),
            }))),
        );
        server.expect(
            Expectation::matching(request::path("/realms/test/certs"))
                .times(..)
                .respond_with(json_encoded(json!({"keys": [jwk]}))),
        );
        Provider { server, key }
    }

    fn issuer(&self) -> String {
        self.server.url_str("/realms/test")
    }

    /// A token for the audience with `scope`, valid for `ttl` seconds
    /// (negative: expired).
    fn token(&self, scope: &str, ttl: i64) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let header = json!({"alg": "ES256", "kid": "k1", "typ": "JWT"});
        let claims = json!({"iss": self.issuer(), "aud": AUDIENCE, "exp": now + ttl,
                            "azp": "test-client", "scope": scope});
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let sig = self
            .key
            .sign(&SystemRandom::new(), input.as_bytes())
            .unwrap();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
    }

    /// The `[serve.auth]` section, with the scopes given.
    fn section(&self, query_scope: Option<&str>, admin_scope: Option<&str>) -> String {
        let mut s = format!(
            "\n[serve.auth]\nissuer = \"{}\"\naudience = \"{AUDIENCE}\"\n",
            self.issuer()
        );
        if let Some(q) = query_scope {
            s.push_str(&format!("query_scope = \"{q}\"\n"));
        }
        if let Some(a) = admin_scope {
            s.push_str(&format!("admin_scope = \"{a}\"\n"));
        }
        s
    }
}

/// A warehouse with `[serve.query]` and `auth` after it. Returns (dir, config).
fn warehouse(auth: &str) -> (tempfile::TempDir, PathBuf) {
    setup(
        NOWHERE,
        &format!("\n[serve.query]\napprove = \"all\"\n{auth}"),
    )
}

/// GET `path` with `token`, if any; (status, WWW-Authenticate, body).
fn get(s: &Serve, path: &str, token: Option<&str>) -> (u16, String, String) {
    let mut req = client().get(format!("{}{path}", s.url));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let r = req.send().unwrap();
    let challenge = r
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    (r.status().as_u16(), challenge, r.text().unwrap())
}

/// The log lines serve has printed so far, waiting a little for the last.
fn log_lines(s: &Serve) -> Vec<String> {
    std::thread::sleep(Duration::from_millis(300));
    s.lines.try_iter().collect()
}

#[test]
fn scopes_decide_which_paths_a_provider_token_opens() {
    let p = Provider::start();
    let (_dir, config) = warehouse(&p.section(Some(QUERY_SCOPE), Some(ADMIN_SCOPE)));
    let s = start_serve(&config, &[]);
    let query = p.token(QUERY_SCOPE, 300);
    let admin = p.token(ADMIN_SCOPE, 300);

    let (status, challenge, _) = get(&s, "/fhir/metadata", None);
    assert_eq!(
        (status, challenge.as_str()),
        (401, r#"Bearer realm="fhir-pipes""#)
    );
    assert_eq!(get(&s, "/fhir/metadata", Some(&query)).0, 200);
    assert_eq!(get(&s, "/fhir/metadata", Some(&admin)).0, 200);

    let (status, challenge, _) = get(&s, "/status", Some(&query));
    assert_eq!(status, 403);
    assert_eq!(
        challenge,
        format!(r#"Bearer realm="fhir-pipes", error="insufficient_scope", scope="{ADMIN_SCOPE}""#)
    );
    assert_eq!(get(&s, "/status", Some(&admin)).0, 200);
    assert_eq!(get(&s, "/status", None).0, 401);

    let (status, challenge, _) = get(&s, "/fhir/metadata", Some(&p.token(QUERY_SCOPE, -600)));
    assert_eq!(status, 401);
    assert!(
        challenge.contains(r#"error="invalid_token""#),
        "{challenge}"
    );
    assert_eq!(
        get(&s, "/fhir/metadata", Some(&p.token("openid", 300))).0,
        403
    );
    assert_eq!(get(&s, "/fhir/metadata", Some("not-a-jwt")).0, 401);
    assert_eq!(get(&s, "/health", None).0, 200);

    let log = log_lines(&s).join("\n");
    assert!(log.contains("auth: token refused: expired"), "{log}");
    assert!(
        log.contains(&format!(
            "auth: token refused for /status: no scope {ADMIN_SCOPE} (client test-client)"
        )),
        "{log}"
    );
    for t in [&query, &admin] {
        assert!(!log.contains(t.as_str()), "a token is in the log: {log}");
    }
}

#[test]
fn the_shared_tokens_still_work_beside_provider_tokens() {
    let p = Provider::start();
    let (_dir, config) = warehouse(&p.section(Some(QUERY_SCOPE), None));
    let s = start_serve(
        &config,
        &[
            ("FHIR_PIPES_SERVE_TOKEN", TOKEN),
            ("FHIR_PIPES_QUERY_TOKEN", "query-test-token"),
        ],
    );
    assert_eq!(get(&s, "/fhir/metadata", Some("query-test-token")).0, 200);
    assert_eq!(get(&s, "/fhir/metadata", Some(TOKEN)).0, 200);
    assert_eq!(get(&s, "/status", Some(TOKEN)).0, 200);
    // Without admin_scope, no provider token opens the run API.
    let token = p.token(&format!("{QUERY_SCOPE} {ADMIN_SCOPE}"), 300);
    assert_eq!(get(&s, "/fhir/metadata", Some(&token)).0, 200);
    let (status, challenge, _) = get(&s, "/status", Some(&token));
    assert_eq!(status, 401);
    assert!(challenge.contains("invalid_token"), "{challenge}");
}

#[test]
fn without_query_scope_any_token_from_the_issuer_queries() {
    let p = Provider::start();
    let (_dir, config) = warehouse(&p.section(None, None));
    let s = start_serve(&config, &[("FHIR_PIPES_SERVE_TOKEN", TOKEN)]);
    assert_eq!(
        get(&s, "/fhir/metadata", Some(&p.token("openid", 300))).0,
        200
    );
    assert_eq!(get(&s, "/fhir/metadata", None).0, 401);
}

#[test]
fn a_non_loopback_listen_with_admin_scope_needs_no_shared_token() {
    let p = Provider::start();
    let (_dir, config) = warehouse(&p.section(Some(QUERY_SCOPE), Some(ADMIN_SCOPE)));
    let mut child = serve_command(&config, "0.0.0.0:0", &[])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    let started = Instant::now();
    while !line.starts_with("listening on ") {
        line.clear();
        if out.read_line(&mut line).unwrap() == 0 {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(line.starts_with("listening on http://0.0.0.0:"), "{line:?}");
}

#[test]
fn a_non_loopback_listen_needs_something_that_guards_the_run_api() {
    let p = Provider::start();
    let (_dir, config) = warehouse(&p.section(Some(QUERY_SCOPE), None));
    let (code, stderr) = serve_fails(&config, "0.0.0.0:0", &[]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("admin_scope"), "{stderr}");
    // The serve token is enough: [serve.auth] guards the query API.
    let (_dir, config) = warehouse(&p.section(Some(QUERY_SCOPE), None));
    let mut child = serve_command(&config, "0.0.0.0:0", &[("FHIR_PIPES_SERVE_TOKEN", TOKEN)])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let _ = child.kill();
    let _ = child.wait();
    assert!(line.starts_with("listening on "), "{line:?}");
}

#[test]
fn an_issuer_that_does_not_match_stops_serve() {
    let p = Provider::start();
    let wrong = p
        .section(None, None)
        .replace("/realms/test", "/realms/test/");
    let (_dir, config) = warehouse(&wrong);
    let (code, stderr) = serve_fails(&config, "127.0.0.1:0", &[]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("names the issuer"), "{stderr}");
}
