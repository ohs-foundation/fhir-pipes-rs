//! Google Cloud: Application Default Credentials and Healthcare API store
//! paths.
//!
//! A Cloud Healthcare API FHIR store is a FHIR server at
//! `https://healthcare.googleapis.com/v1/projects/<p>/locations/<l>/datasets/<d>/fhirStores/<s>/fhir`.
//! `server_url` expands the short store path to that URL, as the Java
//! pipeline accepts it.
//!
//! `Credentials::application_default` finds credentials in the order
//! Google's client libraries use:
//!
//! 1. the JSON file named by `GOOGLE_APPLICATION_CREDENTIALS`: a service
//!    account key, whose token requests are a JWT signed with the key (RS256)
//!    and traded at the key's `token_uri` (the JWT bearer grant), or an
//!    `authorized_user` file, whose refresh token is traded instead;
//! 2. gcloud's user credentials, `application_default_credentials.json` in
//!    the gcloud config directory (`$CLOUDSDK_CONFIG`, else
//!    `~/.config/gcloud`, or `%APPDATA%\gcloud` on Windows), written by
//!    `gcloud auth application-default login`;
//! 3. the metadata server of Compute Engine, Cloud Run and GKE
//!    (`$GCE_METADATA_HOST`, else `metadata.google.internal`).
//!
//! The FHIR client (`client.rs`) requests and renews the tokens. Nothing here
//! prints a token, a refresh token or key material: `describe` names the
//! source only.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;
use serde_json::json;

use crate::error::{PipesError, Result};

/// The scope asked for: all Google Cloud APIs the account's IAM roles allow.
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const HEALTHCARE_API: &str = "https://healthcare.googleapis.com/v1";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const DEFAULT_METADATA_HOST: &str = "metadata.google.internal";
const METADATA_TOKEN_PATH: &str = "/computeMetadata/v1/instance/service-accounts/default/token";
/// The longest lifetime Google accepts for a JWT bearer assertion.
const ASSERTION_LIFETIME: u64 = 3600;
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// A Healthcare API FHIR store path
/// (`projects/<p>/locations/<l>/datasets/<d>/fhirStores/<s>`) as its FHIR
/// base URL; anything else unchanged.
pub fn server_url(server: &str) -> String {
    let path = server.trim_end_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    let is_store = parts.len() == 8
        && ["projects", "locations", "datasets", "fhirStores"]
            .iter()
            .enumerate()
            .all(|(i, name)| parts[2 * i] == *name && !parts[2 * i + 1].is_empty());
    if is_store {
        format!("{HEALTHCARE_API}/{path}/fhir")
    } else {
        server.to_string()
    }
}

/// `server_url` as a clap value parser, for `--server`. A user name or
/// password in the URL is refused: request URLs are built from it, and
/// errors and reports print them.
pub fn parse_server(server: &str) -> std::result::Result<String, String> {
    if url::Url::parse(server).is_ok_and(|u| !u.username().is_empty() || u.password().is_some()) {
        return Err(
            "the URL has a user name or password, which would be printed wherever the URL \
             is; give them with --username and --password (FHIR_PIPES_USERNAME and \
             FHIR_PIPES_PASSWORD) instead"
                .into(),
        );
    }
    Ok(server_url(server))
}

/// Where Google tokens come from.
#[derive(Clone)]
pub struct Credentials {
    pub(crate) source: Source,
    /// Sent as `x-goog-user-project`: the project billed for the requests,
    /// which some APIs need with user credentials. From the file's
    /// `quota_project_id`, or `$GOOGLE_CLOUD_QUOTA_PROJECT`.
    pub(crate) quota_project: Option<String>,
    /// Where they were found, for messages.
    origin: String,
}

#[derive(Clone)]
pub(crate) enum Source {
    /// A service account key: each token request is a newly signed JWT.
    ServiceAccount(ServiceAccount),
    /// A refresh token, traded at `token_uri` with this form.
    RefreshToken { token_uri: String, form: String },
    /// The metadata server's token URL.
    Metadata { url: String },
}

/// A service account's identity and private key.
#[derive(Clone)]
pub(crate) struct ServiceAccount {
    client_email: String,
    key_id: Option<String>,
    key: Arc<RsaKeyPair>,
    pub(crate) token_uri: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credentials({})", self.describe())
    }
}

/// The fields of a credentials file that are read. Not `Debug`: it holds
/// secrets.
#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "type")]
    kind: Option<String>,
    client_email: Option<String>,
    private_key: Option<String>,
    private_key_id: Option<String>,
    token_uri: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
    quota_project_id: Option<String>,
}

impl Credentials {
    /// Application Default Credentials, from the process environment.
    pub fn application_default() -> Result<Self> {
        Self::discover(|name| std::env::var(name).ok())
    }

    /// Application Default Credentials, with `env` in place of the process
    /// environment. A missing file named by GOOGLE_APPLICATION_CREDENTIALS is
    /// an error, not a reason to try the next source.
    pub fn discover(env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let env = |name: &str| env(name).filter(|v| !v.is_empty());
        let quota = env("GOOGLE_CLOUD_QUOTA_PROJECT");
        let mut creds = if let Some(path) = env("GOOGLE_APPLICATION_CREDENTIALS") {
            Self::from_file(Path::new(&path)).map_err(|e| {
                PipesError::Environment(format!("GOOGLE_APPLICATION_CREDENTIALS: {e}"))
            })?
        } else if let Some(path) = gcloud_credentials_path(&env).filter(|p| p.is_file()) {
            Self::from_file(&path).map_err(PipesError::Environment)?
        } else {
            let host = env("GCE_METADATA_HOST").unwrap_or_else(|| DEFAULT_METADATA_HOST.into());
            Credentials {
                source: Source::Metadata {
                    url: format!("http://{host}{METADATA_TOKEN_PATH}"),
                },
                quota_project: None,
                origin: format!(
                    "the metadata server at {host} (GOOGLE_APPLICATION_CREDENTIALS is not set \
                     and there are no gcloud application default credentials)"
                ),
            }
        };
        if quota.is_some() {
            creds.quota_project = quota;
        }
        Ok(creds)
    }

    /// A service account key or `authorized_user` file.
    pub fn from_file(path: &Path) -> std::result::Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_json(&text, &path.display().to_string())
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parse a credentials file's contents; `origin` names it in messages.
    pub fn from_json(text: &str, origin: &str) -> std::result::Result<Self, String> {
        let file: CredentialsFile =
            serde_json::from_str(text).map_err(|e| format!("not a credentials file: {e}"))?;
        let token_uri = file
            .token_uri
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| DEFAULT_TOKEN_URI.into());
        let required = |v: Option<String>, name: &str| {
            v.filter(|v| !v.is_empty())
                .ok_or_else(|| format!("no {name}"))
        };
        let source = match file.kind.as_deref() {
            Some("service_account") => {
                let client_email = required(file.client_email, "client_email")?;
                let key = rsa_key(&required(file.private_key, "private_key")?)?;
                Source::ServiceAccount(ServiceAccount {
                    client_email,
                    key_id: file.private_key_id.filter(|k| !k.is_empty()),
                    key: Arc::new(key),
                    token_uri,
                })
            }
            Some("authorized_user") => {
                let form = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("grant_type", "refresh_token")
                    .append_pair("client_id", &required(file.client_id, "client_id")?)
                    .append_pair(
                        "client_secret",
                        &required(file.client_secret, "client_secret")?,
                    )
                    .append_pair(
                        "refresh_token",
                        &required(file.refresh_token, "refresh_token")?,
                    )
                    .finish();
                Source::RefreshToken { token_uri, form }
            }
            Some(other) => {
                return Err(format!(
                    "credentials of type {other:?} are not supported; use a service account key \
                     or `gcloud auth application-default login`"
                ));
            }
            None => return Err("no \"type\" (service_account or authorized_user)".into()),
        };
        let origin = match &source {
            Source::ServiceAccount(sa) => format!("service account {} ({origin})", sa.client_email),
            _ => format!("user credentials ({origin})"),
        };
        Ok(Credentials {
            source,
            quota_project: file.quota_project_id.filter(|p| !p.is_empty()),
            origin,
        })
    }

    /// Where the tokens come from, without any secret.
    pub fn describe(&self) -> String {
        self.origin.clone()
    }

    /// Whether the tokens come from the metadata server.
    pub fn is_metadata(&self) -> bool {
        matches!(self.source, Source::Metadata { .. })
    }
}

/// gcloud's `application_default_credentials.json`.
fn gcloud_credentials_path(env: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    const FILE: &str = "application_default_credentials.json";
    if let Some(dir) = env("CLOUDSDK_CONFIG") {
        return Some(Path::new(&dir).join(FILE));
    }
    if cfg!(windows) {
        env("APPDATA").map(|d| Path::new(&d).join("gcloud").join(FILE))
    } else {
        env("HOME").map(|d| Path::new(&d).join(".config").join("gcloud").join(FILE))
    }
}

/// An RSA private key from PEM: PKCS #8 (`PRIVATE KEY`, as in Google's key
/// files) or PKCS #1 (`RSA PRIVATE KEY`). Text outside the PEM block is
/// ignored.
fn rsa_key(pem: &str) -> std::result::Result<RsaKeyPair, String> {
    let (label, der) = pem_block(pem).ok_or("private_key is not a PEM private key")?;
    let key = match label {
        "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
        other => return Err(format!("private_key is a {other}, not an RSA private key")),
    };
    key.map_err(|e| format!("private_key is not a usable RSA key: {e}"))
}

/// The label and decoded contents of the first PEM block in `text`.
pub(crate) fn pem_block(text: &str) -> Option<(&str, Vec<u8>)> {
    let begin = text.find("-----BEGIN ")?;
    let rest = &text[begin + "-----BEGIN ".len()..];
    let label_end = rest.find("-----")?;
    let label = &rest[..label_end];
    let body_start = &rest[label_end + 5..];
    let end = body_start.find(&format!("-----END {label}-----"))?;
    let b64: String = body_start[..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    Some((label, STANDARD.decode(b64).ok()?))
}

impl ServiceAccount {
    /// A signed JWT asking for a cloud-platform token, issued at `now`
    /// (seconds since the Unix epoch) and valid for an hour.
    pub(crate) fn assertion(&self, now: u64) -> std::result::Result<String, String> {
        let mut header = json!({"alg": "RS256", "typ": "JWT"});
        if let Some(kid) = &self.key_id {
            header["kid"] = json!(kid);
        }
        let claims = json!({
            "iss": self.client_email,
            "scope": CLOUD_PLATFORM_SCOPE,
            "aud": self.token_uri,
            "iat": now,
            "exp": now + ASSERTION_LIFETIME,
        });
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| "signing the token request failed".to_string())?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }

    /// The token request's form: the JWT bearer grant with a new assertion.
    pub(crate) fn token_form(&self) -> std::result::Result<String, String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "the system clock is before 1970".to_string())?
            .as_secs();
        Ok(url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", JWT_BEARER)
            .append_pair("assertion", &self.assertion(now)?)
            .finish())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
    use serde_json::Value;
    use std::collections::HashMap;

    /// The test-only key under tests/fixtures/google. It grants access to
    /// nothing.
    pub(crate) const TEST_KEY: &str =
        include_str!("../../tests/fixtures/google/test-only-service-account-key.pem");
    const TEST_PUBLIC_KEY: &str =
        include_str!("../../tests/fixtures/google/test-only-service-account-key.pub.pem");
    pub(crate) const TEST_EMAIL: &str = "pipes-test@test-project.iam.gserviceaccount.com";

    /// A service account key file's JSON with the test key.
    pub(crate) fn service_account_json(token_uri: &str) -> String {
        json!({
            "type": "service_account",
            "project_id": "test-project",
            "private_key_id": "test-key-id",
            "private_key": TEST_KEY,
            "client_email": TEST_EMAIL,
            "client_id": "123",
            "token_uri": token_uri,
        })
        .to_string()
    }

    pub(crate) fn authorized_user_json(token_uri: Option<&str>) -> String {
        let mut v = json!({
            "type": "authorized_user",
            "client_id": "user-client.apps.googleusercontent.com",
            "client_secret": "user-secret",
            "refresh_token": "1//refresh-me",
            "quota_project_id": "billing-project",
        });
        if let Some(uri) = token_uri {
            v["token_uri"] = json!(uri);
        }
        v.to_string()
    }

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn decode(part: &str) -> Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    #[test]
    fn a_store_path_expands_to_its_fhir_url() {
        assert_eq!(
            server_url("projects/p1/locations/us-central1/datasets/d1/fhirStores/s1"),
            "https://healthcare.googleapis.com/v1/projects/p1/locations/us-central1/datasets/d1/fhirStores/s1/fhir"
        );
        assert_eq!(
            server_url("projects/example.com:p1/locations/l/datasets/d/fhirStores/s/"),
            "https://healthcare.googleapis.com/v1/projects/example.com:p1/locations/l/datasets/d/fhirStores/s/fhir"
        );
        assert!(
            parse_server("https://u:p@h.org/fhir")
                .unwrap_err()
                .contains("--username")
        );
        assert_eq!(
            parse_server("https://h.org/fhir").unwrap(),
            "https://h.org/fhir"
        );
        for unchanged in [
            "http://localhost:8080/fhir",
            "https://healthcare.googleapis.com/v1/projects/p/locations/l/datasets/d/fhirStores/s/fhir",
            "projects/p/locations/l/datasets/d",
            "projects/p/locations/l/datasets//fhirStores/s",
            "projects/p/locations/l/datasets/d/fhirStores/s/fhir",
            "projects/p/regions/l/datasets/d/fhirStores/s",
        ] {
            assert_eq!(server_url(unchanged), unchanged);
        }
    }

    #[test]
    fn the_assertion_is_a_signed_rs256_jwt_for_the_cloud_platform_scope() {
        let creds = Credentials::from_json(
            &service_account_json("https://oauth2.example/token"),
            "key.json",
        )
        .unwrap();
        let Source::ServiceAccount(sa) = &creds.source else {
            panic!("expected a service account");
        };
        let jwt = sa.assertion(1_700_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "{jwt}");
        assert_eq!(
            decode(parts[0]),
            json!({"alg": "RS256", "typ": "JWT", "kid": "test-key-id"})
        );
        assert_eq!(
            decode(parts[1]),
            json!({
                "iss": TEST_EMAIL,
                "scope": "https://www.googleapis.com/auth/cloud-platform",
                "aud": "https://oauth2.example/token",
                "iat": 1_700_000_000u64,
                "exp": 1_700_003_600u64,
            })
        );
        // The signature verifies with the public key kept apart from the
        // private one.
        let (label, public) = pem_block(TEST_PUBLIC_KEY).unwrap();
        assert_eq!(label, "RSA PUBLIC KEY");
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        let signed = format!("{}.{}", parts[0], parts[1]);
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &public)
            .verify(signed.as_bytes(), &signature)
            .expect("the signature verifies");
        // A changed claim does not.
        let forged = format!("{}.{}", parts[0], parts[0]);
        assert!(
            UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &public)
                .verify(forged.as_bytes(), &signature)
                .is_err()
        );
    }

    #[test]
    fn the_token_form_is_the_jwt_bearer_grant() {
        let creds =
            Credentials::from_json(&service_account_json("https://t.example/token"), "k").unwrap();
        let Source::ServiceAccount(sa) = &creds.source else {
            panic!("expected a service account");
        };
        let form = sa.token_form().unwrap();
        let pairs: HashMap<String, String> = url::form_urlencoded::parse(form.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(pairs["grant_type"], JWT_BEARER);
        assert_eq!(pairs["assertion"].split('.').count(), 3);
    }

    #[test]
    fn google_application_credentials_comes_first() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key.json");
        std::fs::write(&key, service_account_json("https://t.example/token")).unwrap();
        let gcloud = dir.path().join("gcloud");
        std::fs::create_dir_all(&gcloud).unwrap();
        std::fs::write(
            gcloud.join("application_default_credentials.json"),
            authorized_user_json(None),
        )
        .unwrap();
        let creds = Credentials::discover(env(&[
            ("GOOGLE_APPLICATION_CREDENTIALS", key.to_str().unwrap()),
            ("CLOUDSDK_CONFIG", gcloud.to_str().unwrap()),
        ]))
        .unwrap();
        assert!(matches!(creds.source, Source::ServiceAccount(_)));
        let described = creds.describe();
        assert!(described.contains(TEST_EMAIL), "{described}");
        assert!(described.contains("key.json"), "{described}");
        assert_eq!(creds.quota_project, None);
    }

    #[test]
    fn gcloud_user_credentials_come_next() {
        let dir = tempfile::tempdir().unwrap();
        let (gcloud, var) = if cfg!(windows) {
            (dir.path().join("gcloud"), "APPDATA")
        } else {
            (dir.path().join(".config").join("gcloud"), "HOME")
        };
        std::fs::create_dir_all(&gcloud).unwrap();
        std::fs::write(
            gcloud.join("application_default_credentials.json"),
            authorized_user_json(None),
        )
        .unwrap();
        // An empty GOOGLE_APPLICATION_CREDENTIALS counts as unset.
        let vars = [
            (var, dir.path().to_str().unwrap()),
            ("GOOGLE_APPLICATION_CREDENTIALS", ""),
        ];
        let creds = Credentials::discover(env(&vars)).unwrap();
        let Source::RefreshToken { token_uri, form } = &creds.source else {
            panic!("expected user credentials");
        };
        assert_eq!(token_uri, DEFAULT_TOKEN_URI);
        let pairs: HashMap<String, String> = url::form_urlencoded::parse(form.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(pairs["grant_type"], "refresh_token");
        assert_eq!(pairs["refresh_token"], "1//refresh-me");
        assert_eq!(pairs["client_id"], "user-client.apps.googleusercontent.com");
        assert_eq!(pairs["client_secret"], "user-secret");
        assert_eq!(creds.quota_project.as_deref(), Some("billing-project"));
        assert!(creds.describe().starts_with("user credentials ("));
    }

    #[test]
    fn cloudsdk_config_names_the_gcloud_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("application_default_credentials.json"),
            authorized_user_json(Some("https://t.example/token")),
        )
        .unwrap();
        let creds = Credentials::discover(env(&[
            ("CLOUDSDK_CONFIG", dir.path().to_str().unwrap()),
            ("HOME", "/nonexistent"),
            ("APPDATA", "/nonexistent"),
            ("GOOGLE_CLOUD_QUOTA_PROJECT", "from-env"),
        ]))
        .unwrap();
        assert!(
            matches!(&creds.source, Source::RefreshToken { token_uri, .. } if token_uri == "https://t.example/token")
        );
        // The environment wins over the file's quota_project_id.
        assert_eq!(creds.quota_project.as_deref(), Some("from-env"));
    }

    #[test]
    fn the_metadata_server_is_the_last_resort() {
        let none = Credentials::discover(env(&[("HOME", "/nonexistent")])).unwrap();
        assert!(none.is_metadata());
        assert!(
            matches!(&none.source, Source::Metadata { url } if url == "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token")
        );
        let local = Credentials::discover(env(&[
            ("CLOUDSDK_CONFIG", "/nonexistent"),
            ("GCE_METADATA_HOST", "127.0.0.1:8099"),
        ]))
        .unwrap();
        assert!(
            matches!(&local.source, Source::Metadata { url } if url == "http://127.0.0.1:8099/computeMetadata/v1/instance/service-accounts/default/token")
        );
        assert!(local.describe().contains("127.0.0.1:8099"));
    }

    #[test]
    fn a_missing_or_bad_credentials_file_is_an_error() {
        let err = Credentials::discover(env(&[(
            "GOOGLE_APPLICATION_CREDENTIALS",
            "/nonexistent/key.json",
        )]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("GOOGLE_APPLICATION_CREDENTIALS"), "{err}");
        assert!(err.contains("/nonexistent/key.json"), "{err}");

        let bad = |text: &str| Credentials::from_json(text, "f").unwrap_err();
        assert!(bad("not json").contains("not a credentials file"));
        assert!(bad(r#"{"client_email": "x"}"#).contains("type"));
        assert!(bad(r#"{"type": "external_account"}"#).contains("not supported"));
        assert!(bad(r#"{"type": "service_account", "private_key": "x"}"#).contains("client_email"));
        assert!(
            bad(r#"{"type": "service_account", "client_email": "a@b", "private_key": "nope"}"#)
                .contains("not a PEM private key")
        );
        assert!(bad(r#"{"type": "authorized_user", "client_id": "c"}"#).contains("client_secret"));
    }

    #[test]
    fn neither_debug_nor_describe_shows_a_secret() {
        for creds in [
            Credentials::from_json(&service_account_json("https://t.example/token"), "k").unwrap(),
            Credentials::from_json(&authorized_user_json(None), "u").unwrap(),
        ] {
            for shown in [format!("{creds:?}"), creds.describe()] {
                for secret in ["PRIVATE KEY", "MII", "user-secret", "refresh-me"] {
                    assert!(!shown.contains(secret), "{shown}");
                }
            }
        }
    }

    #[test]
    fn a_pem_block_is_found_after_explanatory_text() {
        let (label, der) = pem_block(TEST_KEY).unwrap();
        assert_eq!(label, "PRIVATE KEY");
        assert!(RsaKeyPair::from_pkcs8(&der).is_ok());
        assert!(pem_block("no pem here").is_none());
    }
}
