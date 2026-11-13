//! `[serve.auth]`: access tokens from an identity provider. A caller sends a
//! JWT the provider signed; serve checks its signature against the keys the
//! provider publishes, its issuer, audience and times, and the scope the
//! path needs. See docs/serve.md and
//! docs/superpowers/specs/2026-11-13-serve-token-auth-design.md.
//!
//! Checking may read the provider's keys over HTTP, so [`Verifier::check`]
//! blocks: the server calls it off the async threads.

use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{self, RsaPublicKeyComponents, UnparsedPublicKey, VerificationAlgorithm};
use serde::Deserialize;
use serde_json::Value;

/// Clock skew allowed on `exp` and `nbf`.
const LEEWAY: i64 = 60;
/// Keys older than this are read again at the next call.
const MAX_KEY_AGE: Duration = Duration::from_secs(3600);
/// The keys are read at most this often.
const MIN_REFETCH: Duration = Duration::from_secs(60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DOCUMENT: u64 = 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthFile {
    issuer: String,
    audience: String,
    query_scope: Option<String>,
    admin_scope: Option<String>,
}

/// The `[serve.auth]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    pub issuer: String,
    pub audience: String,
    /// Needed to open `/fhir`; without it any valid token does.
    pub query_scope: Option<String>,
    /// Opens every path; without it a token never opens the run API.
    pub admin_scope: Option<String>,
}

pub fn parse(section: toml::Table) -> Result<AuthConfig> {
    let f: AuthFile = toml::Value::Table(section)
        .try_into()
        .map_err(|e| anyhow::anyhow!("[serve.auth]: {e}"))?;
    let Ok(url) = url::Url::parse(&f.issuer) else {
        bail!("[serve.auth] issuer = \"{}\" is not a URL", f.issuer);
    };
    let loopback = match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback) {
        bail!("[serve.auth] issuer must be an https URL (http only on a loopback host)");
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() {
        bail!("[serve.auth] issuer must not have a user name, password or query");
    }
    if f.audience.trim().is_empty() {
        bail!("[serve.auth] audience must not be empty");
    }
    for (key, scope) in [
        ("query_scope", &f.query_scope),
        ("admin_scope", &f.admin_scope),
    ] {
        // RFC 6749's scope-token: printable ASCII but space, " and \.
        if let Some(s) = scope
            && (s.is_empty()
                || !s
                    .chars()
                    .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\'))
        {
            bail!("[serve.auth] {key} = \"{s}\": one scope, with no spaces, quotes or backslashes");
        }
    }
    Ok(AuthConfig {
        issuer: f.issuer,
        audience: f.audience,
        query_scope: f.query_scope,
        admin_scope: f.admin_scope,
    })
}

/// What a path needs a token to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// `/fhir`: `query_scope` (any valid token without it) or `admin_scope`.
    Query,
    /// The run API: `admin_scope`.
    Admin,
}

/// The outcome of checking a token. Reasons are for serve's log only.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Accepted,
    /// Not a valid token from the issuer for the audience.
    Invalid(&'static str),
    /// Valid, but without the scope the path needs: the scope and the
    /// token's client, for the log.
    Insufficient {
        scope: String,
        client: Option<String>,
    },
}

/// One of the issuer's signing keys, as its JWKS has it.
#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    alg: Option<String>,
    #[serde(rename = "use")]
    use_: Option<String>,
    crv: Option<String>,
    n: Option<String>,
    e: Option<String>,
    x: Option<String>,
    y: Option<String>,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Value>,
}

struct Keys {
    keys: Vec<Jwk>,
    fetched: Instant,
    last_try: Instant,
}

/// Checks tokens against one issuer.
pub struct Verifier {
    cfg: AuthConfig,
    jwks_uri: String,
    /// The issuer's OpenID configuration, as read at startup.
    provider: Value,
    http: reqwest::blocking::Client,
    keys: Mutex<Keys>,
}

impl Verifier {
    /// Read the issuer's OpenID configuration and its keys. Fails when
    /// either cannot be read, or the configuration names another issuer.
    pub fn discover(cfg: AuthConfig) -> Result<Verifier> {
        crate::http::ensure_crypto_provider();
        let http = reqwest::blocking::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .context("building the HTTP client for [serve.auth]")?;
        let url = format!(
            "{}/.well-known/openid-configuration",
            cfg.issuer.trim_end_matches('/')
        );
        let doc = fetch_json(&http, &url)
            .with_context(|| format!("[serve.auth]: reading the issuer's configuration {url}"))?;
        let named = doc.get("issuer").and_then(Value::as_str).unwrap_or("");
        if named != cfg.issuer {
            bail!(
                "[serve.auth] issuer = \"{}\", but {url} names the issuer \"{named}\"; \
                 use that exactly",
                cfg.issuer
            );
        }
        let Some(jwks_uri) = doc.get("jwks_uri").and_then(Value::as_str) else {
            bail!("[serve.auth]: {url} has no jwks_uri");
        };
        let jwks_uri = jwks_uri.to_string();
        let keys = fetch_keys(&http, &jwks_uri)
            .with_context(|| format!("[serve.auth]: reading the issuer's keys {jwks_uri}"))?;
        let now = Instant::now();
        Ok(Verifier {
            cfg,
            jwks_uri,
            provider: doc,
            http,
            keys: Mutex::new(Keys {
                keys,
                fetched: now,
                last_try: now,
            }),
        })
    }

    pub fn config(&self) -> &AuthConfig {
        &self.cfg
    }

    /// The issuer's token endpoint, if its configuration names one.
    pub fn token_endpoint(&self) -> Option<&str> {
        self.provider.get("token_endpoint").and_then(Value::as_str)
    }

    /// The SMART configuration (`/.well-known/smart-configuration`) for
    /// SMART Backend Services, built from the issuer's OpenID
    /// configuration; `None` when it names no token endpoint.
    pub fn smart_configuration(&self) -> Option<Value> {
        let token_endpoint = self.token_endpoint()?;
        let strings = |key: &str| -> Option<Vec<String>> {
            let list: Vec<String> = self
                .provider
                .get(key)?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect();
            (!list.is_empty()).then_some(list)
        };
        let methods = strings("token_endpoint_auth_methods_supported")
            .unwrap_or_else(|| vec!["private_key_jwt".into()]);
        let algs = strings("token_endpoint_auth_signing_alg_values_supported")
            .unwrap_or_else(|| vec!["RS384".into(), "ES384".into()]);
        let mut capabilities = Vec::new();
        if methods.iter().any(|m| m == "private_key_jwt") {
            capabilities.push("client-confidential-asymmetric");
        }
        if methods
            .iter()
            .any(|m| m == "client_secret_basic" || m == "client_secret_post")
        {
            capabilities.push("client-confidential-symmetric");
        }
        let scopes: Vec<&String> = self
            .cfg
            .query_scope
            .iter()
            .chain(&self.cfg.admin_scope)
            .collect();
        Some(serde_json::json!({
            "issuer": self.cfg.issuer,
            "jwks_uri": self.jwks_uri,
            "token_endpoint": token_endpoint,
            "grant_types_supported": ["client_credentials"],
            "token_endpoint_auth_methods_supported": methods,
            "token_endpoint_auth_signing_alg_values_supported": algs,
            "scopes_supported": scopes,
            "capabilities": capabilities,
        }))
    }

    /// Check `token` for a path that needs `need`.
    pub fn check(&self, token: &str, need: Need) -> Verdict {
        let claims = match self.verify(token, now()) {
            Ok(c) => c,
            Err(reason) => return Verdict::Invalid(reason),
        };
        let scopes = scopes(&claims);
        let has = |s: &Option<String>| s.as_ref().is_some_and(|s| grants(&scopes, s));
        let admin = has(&self.cfg.admin_scope);
        let ok = match need {
            Need::Admin => admin,
            Need::Query => admin || self.cfg.query_scope.is_none() || has(&self.cfg.query_scope),
        };
        if ok {
            return Verdict::Accepted;
        }
        let scope = match need {
            Need::Admin => self.cfg.admin_scope.clone(),
            Need::Query => self.cfg.query_scope.clone(),
        };
        Verdict::Insufficient {
            scope: scope.unwrap_or_default(),
            client: client(&claims),
        }
    }

    /// The token's claims if its signature, issuer, audience and times are
    /// good at `now` (seconds since the epoch).
    fn verify(&self, token: &str, now: i64) -> std::result::Result<Value, &'static str> {
        let mut parts = token.split('.');
        let (Some(h), Some(p), Some(s), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("not a JWT");
        };
        let header: Value = decode_json(h).ok_or("a header that is not base64url JSON")?;
        let alg = header
            .get("alg")
            .and_then(Value::as_str)
            .ok_or("no alg in the header")?;
        let kty = key_type(alg).ok_or("an algorithm that is not accepted")?;
        let kid = header.get("kid").and_then(Value::as_str);
        let sig = URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|_| "a signature that is not base64url")?;
        let key = self
            .key(kid, kty, alg)
            .ok_or("signed with a key the issuer does not publish")?;
        let signed = &token.as_bytes()[..h.len() + 1 + p.len()];
        verify_signature(alg, &key, signed, &sig)?;
        let claims: Value = decode_json(p).ok_or("claims that are not base64url JSON")?;
        check_claims(&claims, &self.cfg, now)?;
        Ok(claims)
    }

    /// The issuer's key for a token, reading the keys again when the token
    /// names one not seen yet or the keys are old.
    fn key(&self, kid: Option<&str>, kty: &str, alg: &str) -> Option<Jwk> {
        let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let found = find_key(&keys.keys, kid, kty, alg);
        let stale = keys.fetched.elapsed() >= MAX_KEY_AGE;
        if found.is_some() && !stale {
            return found;
        }
        if keys.last_try.elapsed() < MIN_REFETCH {
            return found;
        }
        keys.last_try = Instant::now();
        match fetch_keys(&self.http, &self.jwks_uri) {
            Ok(fresh) => {
                keys.keys = fresh;
                keys.fetched = Instant::now();
                find_key(&keys.keys, kid, kty, alg)
            }
            // The provider is down: keep the keys there are.
            Err(_) => found,
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn fetch_json(http: &reqwest::blocking::Client, url: &str) -> Result<Value> {
    let resp = http
        .get(url)
        .header("accept", "application/json")
        .send()
        .map_err(|e| anyhow::anyhow!("{}", crate::redact::urls(&e.to_string())))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("HTTP {}", status.as_u16());
    }
    let mut body = Vec::new();
    resp.take(MAX_DOCUMENT + 1)
        .read_to_end(&mut body)
        .context("reading the response")?;
    if body.len() as u64 > MAX_DOCUMENT {
        bail!("the response is over {MAX_DOCUMENT} bytes");
    }
    serde_json::from_slice(&body).context("the response is not JSON")
}

/// The keys of a JWKS that can check signatures. A key of a type we do not
/// check, or that does not parse, is left out.
fn fetch_keys(http: &reqwest::blocking::Client, url: &str) -> Result<Vec<Jwk>> {
    let jwks: Jwks =
        serde_json::from_value(fetch_json(http, url)?).context("the response is not a JWK Set")?;
    Ok(jwks
        .keys
        .into_iter()
        .filter_map(|k| serde_json::from_value::<Jwk>(k).ok())
        .filter(|k| k.use_.as_deref().is_none_or(|u| u == "sig"))
        .collect())
}

/// The key type an accepted algorithm uses; `None` for any other, which
/// covers `none` and the HMAC algorithms.
fn key_type(alg: &str) -> Option<&'static str> {
    match alg {
        "RS256" | "RS384" | "RS512" | "PS256" | "PS384" | "PS512" => Some("RSA"),
        "ES256" | "ES384" => Some("EC"),
        "EdDSA" => Some("OKP"),
        _ => None,
    }
}

/// The key named `kid`, or without a `kid` the only key of the type.
fn find_key(keys: &[Jwk], kid: Option<&str>, kty: &str, alg: &str) -> Option<Jwk> {
    let usable = |k: &&Jwk| k.kty == kty && k.alg.as_deref().is_none_or(|a| a == alg);
    match kid {
        Some(kid) => keys
            .iter()
            .filter(usable)
            .find(|k| k.kid.as_deref() == Some(kid))
            .cloned(),
        None => {
            let mut of_type = keys.iter().filter(usable);
            match (of_type.next(), of_type.next()) {
                (Some(k), None) => Some(k.clone()),
                _ => None,
            }
        }
    }
}

fn b64(field: &Option<String>) -> std::result::Result<Vec<u8>, &'static str> {
    field
        .as_deref()
        .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
        .ok_or("a key the issuer publishes that does not parse")
}

fn verify_signature(
    alg: &str,
    key: &Jwk,
    message: &[u8],
    sig: &[u8],
) -> std::result::Result<(), &'static str> {
    let bad = "a signature that does not match the issuer's key";
    match key.kty.as_str() {
        "RSA" => {
            let params: &signature::RsaParameters = match alg {
                "RS256" => &signature::RSA_PKCS1_2048_8192_SHA256,
                "RS384" => &signature::RSA_PKCS1_2048_8192_SHA384,
                "RS512" => &signature::RSA_PKCS1_2048_8192_SHA512,
                "PS256" => &signature::RSA_PSS_2048_8192_SHA256,
                "PS384" => &signature::RSA_PSS_2048_8192_SHA384,
                "PS512" => &signature::RSA_PSS_2048_8192_SHA512,
                _ => return Err(bad),
            };
            let (n, e) = (b64(&key.n)?, b64(&key.e)?);
            RsaPublicKeyComponents { n, e }
                .verify(params, message, sig)
                .map_err(|_| bad)
        }
        "EC" => {
            let (algorithm, crv, len): (&dyn VerificationAlgorithm, _, _) = match alg {
                "ES256" => (&signature::ECDSA_P256_SHA256_FIXED, "P-256", 32),
                "ES384" => (&signature::ECDSA_P384_SHA384_FIXED, "P-384", 48),
                _ => return Err(bad),
            };
            let (x, y) = (b64(&key.x)?, b64(&key.y)?);
            if key.crv.as_deref() != Some(crv) || x.len() != len || y.len() != len {
                return Err("a key the issuer publishes that does not parse");
            }
            let point = [&[4u8][..], &x, &y].concat();
            UnparsedPublicKey::new(algorithm, point)
                .verify(message, sig)
                .map_err(|_| bad)
        }
        "OKP" => {
            if alg != "EdDSA" || key.crv.as_deref() != Some("Ed25519") {
                return Err(bad);
            }
            UnparsedPublicKey::new(&signature::ED25519, b64(&key.x)?)
                .verify(message, sig)
                .map_err(|_| bad)
        }
        _ => Err(bad),
    }
}

fn decode_json(part: &str) -> Option<Value> {
    let bytes = URL_SAFE_NO_PAD.decode(part).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn check_claims(
    claims: &Value,
    cfg: &AuthConfig,
    now: i64,
) -> std::result::Result<(), &'static str> {
    if !claims.is_object() {
        return Err("claims that are not a JSON object");
    }
    if claims.get("iss").and_then(Value::as_str) != Some(cfg.issuer.as_str()) {
        return Err("another issuer");
    }
    let aud_ok = match claims.get("aud") {
        Some(Value::String(a)) => *a == cfg.audience,
        Some(Value::Array(a)) => a.iter().any(|v| v.as_str() == Some(cfg.audience.as_str())),
        _ => false,
    };
    if !aud_ok {
        return Err("another audience");
    }
    let time = |name: &str| claims.get(name).and_then(Value::as_f64).map(|t| t as i64);
    match time("exp") {
        None => return Err("no exp"),
        Some(exp) if now > exp + LEEWAY => return Err("expired"),
        Some(_) => {}
    }
    if let Some(nbf) = time("nbf")
        && now + LEEWAY < nbf
    {
        return Err("not valid yet (nbf)");
    }
    Ok(())
}

/// The token's scopes and roles: `scope` (space-separated), `scp` (a
/// string or an array) and `roles` (an array).
fn scopes(claims: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut add = |v: Option<&Value>, split: bool| match v {
        Some(Value::String(s)) if split => out.extend(s.split_whitespace().map(String::from)),
        Some(Value::String(s)) => out.push(s.clone()),
        Some(Value::Array(a)) => out.extend(a.iter().filter_map(Value::as_str).map(String::from)),
        _ => {}
    };
    add(claims.get("scope"), true);
    add(claims.get("scp"), true);
    add(claims.get("roles"), false);
    out
}

/// Whether a token with `held` scopes has `wanted`. A SMART scope wanted
/// (`system/Library.rs`) is granted by one of the same context whose
/// resource is the same or `*` and whose permissions include its own; any
/// other scope only by itself.
fn grants(held: &[String], wanted: &str) -> bool {
    match Smart::parse(wanted) {
        Some(w) => held
            .iter()
            .filter_map(|h| Smart::parse(h))
            .any(|h| h.grants(&w)),
        None => held.iter().any(|h| h == wanted),
    }
}

/// A SMART on FHIR scope, `<context>/<resource>.<permissions>`, without a
/// query part.
#[derive(Debug, PartialEq, Eq)]
struct Smart<'a> {
    context: &'a str,
    resource: &'a str,
    /// The SMART v2 permissions, a subset of `cruds`; SMART v1's `read` is
    /// `rs`, `write` is `cud` and `*` is `cruds`.
    permissions: Vec<char>,
}

impl<'a> Smart<'a> {
    fn parse(scope: &'a str) -> Option<Smart<'a>> {
        // A query part (SMART v2's `?category=…`) narrows a scope: it is
        // not parsed, so it grants nothing that is wanted without one.
        if scope.contains('?') {
            return None;
        }
        let (context, rest) = scope.split_once('/')?;
        if !matches!(context, "patient" | "user" | "system") {
            return None;
        }
        let (resource, permissions) = rest.split_once('.')?;
        let resource_ok = resource == "*"
            || resource
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
                && resource.chars().all(|c| c.is_ascii_alphanumeric());
        if !resource_ok {
            return None;
        }
        let permissions: Vec<char> = match permissions {
            "read" => vec!['r', 's'],
            "write" => vec!['c', 'u', 'd'],
            "*" => "cruds".chars().collect(),
            p => {
                // SMART v2: some of c, r, u, d, s, in that order, each once.
                let mut order = "cruds".chars();
                if p.is_empty() || !p.chars().all(|c| order.any(|o| o == c)) {
                    return None;
                }
                p.chars().collect()
            }
        };
        Some(Smart {
            context,
            resource,
            permissions,
        })
    }

    fn grants(&self, wanted: &Smart) -> bool {
        self.context == wanted.context
            && (self.resource == "*" || self.resource == wanted.resource)
            && wanted
                .permissions
                .iter()
                .all(|p| self.permissions.contains(p))
    }
}

/// The client a token was issued to, for the log: `azp` or `client_id`,
/// cut short and with only printable characters.
fn client(claims: &Value) -> Option<String> {
    let c = ["azp", "client_id"]
        .iter()
        .find_map(|k| claims.get(*k).and_then(Value::as_str))?;
    Some(
        c.chars()
            .filter(|c| c.is_ascii_graphic())
            .take(100)
            .collect(),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use httptest::matchers::request;
    use httptest::responders::{json_encoded, status_code};
    use httptest::{Expectation, Server};
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair};
    use serde_json::json;

    pub(crate) const AUDIENCE: &str = "fhir-pipes";

    fn enc(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// A signing key of each type, with its public JWK.
    pub(crate) enum TestKey {
        Ec(EcdsaKeyPair),
        Ed(Ed25519KeyPair),
        Rsa(ring::signature::RsaKeyPair),
    }

    impl TestKey {
        pub(crate) fn ec() -> TestKey {
            let rng = SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            TestKey::Ec(
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap(),
            )
        }

        fn ed() -> TestKey {
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
            TestKey::Ed(Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap())
        }

        fn rsa() -> TestKey {
            let (_, der) =
                crate::http::google::pem_block(crate::http::google::tests::TEST_KEY).unwrap();
            TestKey::Rsa(ring::signature::RsaKeyPair::from_pkcs8(&der).unwrap())
        }

        pub(crate) fn alg(&self) -> &'static str {
            match self {
                TestKey::Ec(_) => "ES256",
                TestKey::Ed(_) => "EdDSA",
                TestKey::Rsa(_) => "RS256",
            }
        }

        pub(crate) fn jwk(&self, kid: &str) -> Value {
            match self {
                TestKey::Ec(k) => {
                    let p = k.public_key().as_ref();
                    json!({"kty": "EC", "crv": "P-256", "kid": kid, "use": "sig",
                           "x": enc(&p[1..33]), "y": enc(&p[33..65])})
                }
                TestKey::Ed(k) => {
                    json!({"kty": "OKP", "crv": "Ed25519", "kid": kid, "x": enc(k.public_key().as_ref())})
                }
                TestKey::Rsa(k) => {
                    let p = ring::rsa::PublicKeyComponents::<Vec<u8>>::from(k.public());
                    json!({"kty": "RSA", "kid": kid, "alg": "RS256",
                           "n": enc(&p.n), "e": enc(&p.e)})
                }
            }
        }

        /// A JWT with `header` (alg added unless given) and `claims`.
        pub(crate) fn sign(&self, mut header: Value, claims: &Value) -> String {
            if header.get("alg").is_none() {
                header["alg"] = json!(self.alg());
            }
            let input = format!(
                "{}.{}",
                enc(header.to_string().as_bytes()),
                enc(claims.to_string().as_bytes())
            );
            let rng = SystemRandom::new();
            let sig = match self {
                TestKey::Ec(k) => k.sign(&rng, input.as_bytes()).unwrap().as_ref().to_vec(),
                TestKey::Ed(k) => k.sign(input.as_bytes()).as_ref().to_vec(),
                TestKey::Rsa(k) => {
                    let mut sig = vec![0; k.public().modulus_len()];
                    k.sign(
                        &signature::RSA_PKCS1_SHA256,
                        &rng,
                        input.as_bytes(),
                        &mut sig,
                    )
                    .unwrap();
                    sig
                }
            };
            format!("{input}.{}", enc(&sig))
        }
    }

    /// A provider serving its OpenID configuration and `jwks` (any number
    /// of times). Returns the server and its issuer URL.
    pub(crate) fn provider(jwks: Value) -> (Server, String) {
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
                "token_endpoint": server.url_str("/realms/test/token"),
                "token_endpoint_auth_methods_supported": ["private_key_jwt", "client_secret_basic"],
            }))),
        );
        server.expect(
            Expectation::matching(request::path("/realms/test/certs"))
                .times(..)
                .respond_with(json_encoded(jwks)),
        );
        (server, issuer)
    }

    pub(crate) fn claims(issuer: &str, scope: &str) -> Value {
        json!({"iss": issuer, "aud": AUDIENCE, "exp": now() + 300, "azp": "test-client", "scope": scope})
    }

    fn config(issuer: &str) -> AuthConfig {
        AuthConfig {
            issuer: issuer.to_string(),
            audience: AUDIENCE.to_string(),
            query_scope: Some("fhir-pipes.query".into()),
            admin_scope: Some("fhir-pipes.admin".into()),
        }
    }

    fn verifier(key: &TestKey) -> (Server, String, Verifier) {
        let (server, issuer) = provider(json!({"keys": [key.jwk("k1")]}));
        let v = Verifier::discover(config(&issuer)).unwrap();
        (server, issuer, v)
    }

    fn invalid(v: &Verifier, token: &str) -> &'static str {
        match v.check(token, Need::Query) {
            Verdict::Invalid(r) => r,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn each_key_type_signs_an_accepted_token() {
        for key in [TestKey::ec(), TestKey::ed(), TestKey::rsa()] {
            let (_s, issuer, v) = verifier(&key);
            let t = key.sign(json!({"kid": "k1"}), &claims(&issuer, "fhir-pipes.query"));
            assert_eq!(v.check(&t, Need::Query), Verdict::Accepted, "{}", key.alg());
        }
    }

    #[test]
    fn a_token_without_kid_uses_the_only_key_of_its_type() {
        let key = TestKey::ec();
        let (_s, issuer, v) = verifier(&key);
        let t = key.sign(json!({}), &claims(&issuer, "fhir-pipes.query"));
        assert_eq!(v.check(&t, Need::Query), Verdict::Accepted);
    }

    #[test]
    fn claims_are_checked() {
        let key = TestKey::ec();
        let (_s, issuer, v) = verifier(&key);
        let sign = |c: Value| key.sign(json!({"kid": "k1"}), &c);
        let base = claims(&issuer, "fhir-pipes.query");
        let with = |k: &str, val: Value| {
            let mut c = base.clone();
            c[k] = val;
            sign(c)
        };
        assert_eq!(
            invalid(&v, &with("iss", json!("https://other.example"))),
            "another issuer"
        );
        assert_eq!(
            invalid(&v, &with("aud", json!("account"))),
            "another audience"
        );
        assert_eq!(
            v.check(&with("aud", json!(["account", AUDIENCE])), Need::Query),
            Verdict::Accepted
        );
        assert_eq!(invalid(&v, &with("exp", json!(now() - 120))), "expired");
        // Within the leeway.
        assert_eq!(
            v.check(&with("exp", json!(now() - 10)), Need::Query),
            Verdict::Accepted
        );
        assert_eq!(
            invalid(&v, &with("nbf", json!(now() + 600))),
            "not valid yet (nbf)"
        );
        let mut no_exp = base.clone();
        no_exp.as_object_mut().unwrap().remove("exp");
        assert_eq!(invalid(&v, &sign(no_exp)), "no exp");
    }

    #[test]
    fn unsigned_and_shared_secret_tokens_are_refused() {
        let key = TestKey::ec();
        let (_s, issuer, v) = verifier(&key);
        let c = claims(&issuer, "fhir-pipes.query");
        let body = format!(
            "{}.{}",
            enc(br#"{"alg":"none"}"#),
            enc(c.to_string().as_bytes())
        );
        assert_eq!(
            invalid(&v, &format!("{body}.")),
            "an algorithm that is not accepted"
        );
        let hs = key.sign(json!({"alg": "HS256", "kid": "k1"}), &c);
        assert_eq!(invalid(&v, &hs), "an algorithm that is not accepted");
    }

    #[test]
    fn a_wrong_signature_or_key_is_refused() {
        let key = TestKey::ec();
        let (_s, issuer, v) = verifier(&key);
        let c = claims(&issuer, "fhir-pipes.query");
        // Another key with the published key's kid.
        let other = TestKey::ec().sign(json!({"kid": "k1"}), &c);
        assert_eq!(
            invalid(&v, &other),
            "a signature that does not match the issuer's key"
        );
        // Claims changed after signing.
        let t = key.sign(json!({"kid": "k1"}), &c);
        let mut parts: Vec<&str> = t.split('.').collect();
        let mut forged = c.clone();
        forged["aud"] = json!(AUDIENCE);
        forged["scope"] = json!("fhir-pipes.admin");
        let forged = enc(forged.to_string().as_bytes());
        parts[1] = &forged;
        assert_eq!(
            invalid(&v, &parts.join(".")),
            "a signature that does not match the issuer's key"
        );
        // An EC key used for an RSA algorithm: no RSA key is published.
        let rs = key.sign(json!({"alg": "RS256", "kid": "k1"}), &c);
        assert_eq!(
            invalid(&v, &rs),
            "signed with a key the issuer does not publish"
        );
        assert_eq!(invalid(&v, "abc"), "not a JWT");
        assert_eq!(invalid(&v, "a.b.c.d"), "not a JWT");
    }

    #[test]
    fn scopes_decide_which_paths_a_token_opens() {
        let key = TestKey::ec();
        let (_s, issuer, v) = verifier(&key);
        let token = |c: Value| key.sign(json!({"kid": "k1"}), &c);
        let with = |claim: &str, val: Value| {
            let mut c = claims(&issuer, "");
            c.as_object_mut().unwrap().remove("scope");
            c[claim] = val;
            token(c)
        };
        let query = with("scope", json!("openid fhir-pipes.query"));
        assert_eq!(v.check(&query, Need::Query), Verdict::Accepted);
        assert_eq!(
            v.check(&query, Need::Admin),
            Verdict::Insufficient {
                scope: "fhir-pipes.admin".into(),
                client: Some("test-client".into())
            }
        );
        for admin in [
            with("scope", json!("fhir-pipes.admin")),
            with("scp", json!(["fhir-pipes.admin"])),
            with("scp", json!("x fhir-pipes.admin")),
            with("roles", json!(["fhir-pipes.admin"])),
        ] {
            assert_eq!(v.check(&admin, Need::Admin), Verdict::Accepted);
            assert_eq!(v.check(&admin, Need::Query), Verdict::Accepted);
        }
        let none = with("scope", json!("openid"));
        assert!(matches!(
            v.check(&none, Need::Query),
            Verdict::Insufficient { .. }
        ));
    }

    #[test]
    fn without_query_scope_any_valid_token_queries_and_without_admin_scope_none_administers() {
        let key = TestKey::ec();
        let (_s, issuer) = provider(json!({"keys": [key.jwk("k1")]}));
        let mut cfg = config(&issuer);
        cfg.query_scope = None;
        cfg.admin_scope = None;
        let v = Verifier::discover(cfg).unwrap();
        let t = key.sign(json!({"kid": "k1"}), &claims(&issuer, "fhir-pipes.admin"));
        assert_eq!(v.check(&t, Need::Query), Verdict::Accepted);
        assert!(matches!(
            v.check(&t, Need::Admin),
            Verdict::Insufficient { .. }
        ));
    }

    #[test]
    fn a_new_kid_reads_the_keys_again_at_most_once_a_minute() {
        let old = TestKey::ec();
        let new = TestKey::ec();
        let server = Server::run();
        let issuer = server.url_str("/realms/test");
        server.expect(
            Expectation::matching(request::path(
                "/realms/test/.well-known/openid-configuration",
            ))
            .respond_with(json_encoded(json!({
                    "issuer": issuer, "jwks_uri": server.url_str("/realms/test/certs")}))),
        );
        // Read at startup, then once for the new kid; the second unknown
        // kid within the minute does not read them again.
        server.expect(
            Expectation::matching(request::path("/realms/test/certs"))
                .times(2)
                .respond_with(httptest::cycle![
                    json_encoded(json!({"keys": [old.jwk("old")]})),
                    json_encoded(json!({"keys": [old.jwk("old"), new.jwk("new")]})),
                ]),
        );
        let v = Verifier::discover(config(&issuer)).unwrap();
        // As if the startup read was over a minute ago.
        v.keys.lock().unwrap().last_try = Instant::now() - MIN_REFETCH;
        let c = claims(&issuer, "fhir-pipes.query");
        let t = new.sign(json!({"kid": "new"}), &c);
        assert_eq!(v.check(&t, Need::Query), Verdict::Accepted);
        let t = TestKey::ec().sign(json!({"kid": "newer"}), &c);
        assert_eq!(
            invalid(&v, &t),
            "signed with a key the issuer does not publish"
        );
    }

    #[test]
    fn discovery_must_name_the_configured_issuer() {
        let key = TestKey::ec();
        let (_s, issuer) = provider(json!({"keys": [key.jwk("k1")]}));
        let err = Verifier::discover(config(&format!("{issuer}/")))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("names the issuer"), "{err}");
    }

    #[test]
    fn discovery_that_fails_is_an_error() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::path(
                "/realms/test/.well-known/openid-configuration",
            ))
            .respond_with(status_code(404)),
        );
        let err = Verifier::discover(config(&server.url_str("/realms/test")))
            .err()
            .unwrap();
        let msg = format!("{err:#}");
        assert!(msg.contains("HTTP 404"), "{msg}");
    }

    #[test]
    fn the_section_is_checked() {
        let p = |s: &str| parse(toml::from_str(s).unwrap());
        let ok = p("issuer = \"https://auth.example.org/realms/icr\"\naudience = \"fhir-pipes\"")
            .unwrap();
        assert_eq!(ok.query_scope, None);
        assert!(p("issuer = \"http://auth.example.org/realms/icr\"\naudience = \"a\"").is_err());
        assert!(p("issuer = \"http://127.0.0.1:9000/realms/x\"\naudience = \"a\"").is_ok());
        assert!(p("issuer = \"https://a.example\"\naudience = \" \"").is_err());
        assert!(p("issuer = \"https://a.example\"").is_err());
        assert!(
            p("issuer = \"https://a.example\"\naudience = \"a\"\nquery_scope = \"a b\"").is_err()
        );
        assert!(p("issuer = \"https://a.example\"\naudience = \"a\"\nscopes = \"x\"").is_err());
    }

    #[test]
    fn smart_scopes_grant_what_they_cover() {
        let g = |held: &[&str], wanted: &str| {
            grants(
                &held.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                wanted,
            )
        };
        let q = "system/Library.rs";
        for held in [
            "system/Library.rs",
            "system/Library.cruds",
            "system/*.rs",
            "system/Library.read",
            "system/*.read",
            "system/*.*",
        ] {
            assert!(g(&[held], q), "{held}");
        }
        for held in [
            "system/Library.r",
            "patient/Library.rs",
            "system/Patient.rs",
            "system/Library.write",
            "system/Library.rs?status=active",
            "system/Library.sr",
            "Library.rs",
            "fhir-pipes.query",
        ] {
            assert!(!g(&[held], q), "{held}");
        }
        assert!(g(
            &["openid", "system/Library.s", "system/Library.r"],
            "system/Library.s"
        ));
        // A scope wanted that is not SMART's is matched exactly.
        assert!(g(&["fhir-pipes.admin"], "fhir-pipes.admin"));
        assert!(!g(&["system/*.*"], "fhir-pipes.admin"));
        // A wanted scope with a query part is not SMART-parsed either.
        assert!(g(&["system/Library.rs?x=1"], "system/Library.rs?x=1"));
        assert!(!g(&["system/*.cruds"], "system/Library.rs?x=1"));
    }

    #[test]
    fn a_smart_query_scope_opens_fhir_for_a_wildcard_token() {
        let key = TestKey::ec();
        let (_s, issuer) = provider(json!({"keys": [key.jwk("k1")]}));
        let mut cfg = config(&issuer);
        cfg.query_scope = Some("system/Library.rs".into());
        let v = Verifier::discover(cfg).unwrap();
        let t = key.sign(json!({"kid": "k1"}), &claims(&issuer, "system/*.read"));
        assert_eq!(v.check(&t, Need::Query), Verdict::Accepted);
        let t = key.sign(json!({"kid": "k1"}), &claims(&issuer, "system/Library.r"));
        assert!(matches!(
            v.check(&t, Need::Query),
            Verdict::Insufficient { .. }
        ));
    }

    #[test]
    fn the_smart_configuration_follows_the_provider() {
        let key = TestKey::ec();
        let (server, _s, v) = verifier(&key);
        let c = v.smart_configuration().unwrap();
        assert_eq!(c["token_endpoint"], server.url_str("/realms/test/token"));
        assert_eq!(c["jwks_uri"], server.url_str("/realms/test/certs"));
        assert_eq!(c["grant_types_supported"], json!(["client_credentials"]));
        assert_eq!(
            c["token_endpoint_auth_methods_supported"],
            json!(["private_key_jwt", "client_secret_basic"])
        );
        assert_eq!(
            c["token_endpoint_auth_signing_alg_values_supported"],
            json!(["RS384", "ES384"])
        );
        assert_eq!(
            c["capabilities"],
            json!([
                "client-confidential-asymmetric",
                "client-confidential-symmetric"
            ])
        );
        assert_eq!(
            c["scopes_supported"],
            json!(["fhir-pipes.query", "fhir-pipes.admin"])
        );
    }

    #[test]
    fn without_a_token_endpoint_there_is_no_smart_configuration() {
        let key = TestKey::ec();
        let server = Server::run();
        let issuer = server.url_str("/i");
        server.expect(
            Expectation::matching(request::path("/i/.well-known/openid-configuration"))
                .respond_with(json_encoded(
                    json!({"issuer": issuer, "jwks_uri": server.url_str("/k")}),
                )),
        );
        server.expect(
            Expectation::matching(request::path("/k"))
                .respond_with(json_encoded(json!({"keys": [key.jwk("k1")]}))),
        );
        let v = Verifier::discover(config(&issuer)).unwrap();
        assert_eq!(v.smart_configuration(), None);
    }

    #[test]
    fn the_client_is_printable_and_short() {
        assert_eq!(
            client(&json!({"azp": "dash\nboards"})),
            Some("dashboards".to_string())
        );
        assert_eq!(
            client(&json!({"client_id": "x".repeat(300)}))
                .unwrap()
                .len(),
            100
        );
        assert_eq!(client(&json!({})), None);
    }
}
