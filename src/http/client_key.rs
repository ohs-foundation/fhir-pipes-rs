//! A client's private key, which signs the assertion it sends to a token
//! endpoint in place of a client secret: `private_key_jwt` (RFC 7523), as
//! SMART on FHIR Backend Services clients authenticate. See
//! docs/superpowers/specs/2026-11-13-serve-smart-design.md.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{
    ECDSA_P256_SHA256_FIXED_SIGNING, ECDSA_P384_SHA384_FIXED_SIGNING, EcdsaKeyPair,
    RSA_PKCS1_SHA384, RsaKeyPair,
};
use serde_json::json;

use crate::error::PipesError;

const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
/// How long an assertion is valid; SMART allows at most five minutes.
const LIFETIME: u64 = 300;

enum Key {
    Rsa(RsaKeyPair),
    Ec(EcdsaKeyPair),
}

/// A private key and the id (`kid`) it is registered under.
pub struct ClientKey {
    key: Key,
    kid: String,
    alg: &'static str,
}

impl std::fmt::Debug for ClientKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key.
        f.debug_struct("ClientKey")
            .field("kid", &self.kid)
            .field("alg", &self.alg)
            .finish()
    }
}

impl ClientKey {
    /// Read a PEM private key file. The error names the file and never
    /// quotes it.
    pub fn read(path: &Path, kid: &str) -> crate::error::Result<ClientKey> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            PipesError::Usage(format!("reading the client key {}: {e}", path.display()))
        })?;
        ClientKey::from_pem(&text, kid)
            .map_err(|e| PipesError::Usage(format!("the client key {}: {e}", path.display())))
    }

    /// An RSA key (PKCS #8 or #1) signs with RS384, an EC P-384 key
    /// (PKCS #8) with ES384 and an EC P-256 key with ES256.
    pub fn from_pem(pem: &str, kid: &str) -> Result<ClientKey, String> {
        if kid.is_empty() {
            return Err("the key id is empty".into());
        }
        let (label, der) = super::google::pem_block(pem).ok_or("not a PEM private key")?;
        let rng = SystemRandom::new();
        let (key, alg) = match label {
            "RSA PRIVATE KEY" => (
                Key::Rsa(
                    RsaKeyPair::from_der(&der).map_err(|e| format!("not a usable RSA key: {e}"))?,
                ),
                "RS384",
            ),
            "PRIVATE KEY" => {
                if let Ok(k) =
                    EcdsaKeyPair::from_pkcs8(&ECDSA_P384_SHA384_FIXED_SIGNING, &der, &rng)
                {
                    (Key::Ec(k), "ES384")
                } else if let Ok(k) =
                    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &rng)
                {
                    (Key::Ec(k), "ES256")
                } else {
                    let k = RsaKeyPair::from_pkcs8(&der)
                        .map_err(|_| "not an RSA, EC P-384 or EC P-256 private key".to_string())?;
                    (Key::Rsa(k), "RS384")
                }
            }
            "EC PRIVATE KEY" => {
                return Err(
                    "an EC key must be PKCS #8 (BEGIN PRIVATE KEY); convert it with \
                            `openssl pkcs8 -topk8 -nocrypt -in key.pem -out key-pkcs8.pem`"
                        .into(),
                );
            }
            other => return Err(format!("a {other}, not a private key")),
        };
        Ok(ClientKey {
            key,
            kid: kid.to_string(),
            alg,
        })
    }

    pub fn alg(&self) -> &'static str {
        self.alg
    }

    /// A signed assertion for `client_id` to send to `token_url`, issued
    /// at `now` (seconds since the epoch).
    pub fn assertion(&self, client_id: &str, token_url: &str, now: u64) -> Result<String, String> {
        let rng = SystemRandom::new();
        let mut jti = [0u8; 16];
        rng.fill(&mut jti)
            .map_err(|_| "no random bytes for the assertion's jti")?;
        let header = json!({"alg": self.alg, "kid": self.kid, "typ": "JWT"});
        let claims = json!({
            "iss": client_id,
            "sub": client_id,
            "aud": token_url,
            "iat": now,
            "exp": now + LIFETIME,
            "jti": jti.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        });
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let sig = match &self.key {
            Key::Rsa(k) => {
                let mut sig = vec![0; k.public().modulus_len()];
                k.sign(&RSA_PKCS1_SHA384, &rng, input.as_bytes(), &mut sig)
                    .map_err(|_| "signing the assertion failed")?;
                sig
            }
            Key::Ec(k) => k
                .sign(&rng, input.as_bytes())
                .map_err(|_| "signing the assertion failed")?
                .as_ref()
                .to_vec(),
        };
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig)))
    }

    /// The token request's form: the client-credentials grant, the scope
    /// when given, and a new assertion.
    pub fn token_form(
        &self,
        client_id: &str,
        token_url: &str,
        scope: Option<&str>,
    ) -> Result<String, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.append_pair("grant_type", "client_credentials");
        if let Some(scope) = scope {
            form.append_pair("scope", scope);
        }
        form.append_pair("client_assertion_type", ASSERTION_TYPE)
            .append_pair(
                "client_assertion",
                &self.assertion(client_id, token_url, now)?,
            );
        Ok(form.finish())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::signature::{
        ECDSA_P256_SHA256_FIXED, ECDSA_P384_SHA384_FIXED, RSA_PKCS1_2048_8192_SHA384,
        UnparsedPublicKey, VerificationAlgorithm,
    };
    use serde_json::Value;

    pub(crate) fn pem(label: &str, der: &[u8]) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(der);
        let lines: Vec<&str> = b64
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        format!(
            "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
            lines.join("\n")
        )
    }

    /// A new EC key as PKCS #8 PEM, with its public key bytes.
    pub(crate) fn ec_pem(p384: bool) -> (String, Vec<u8>) {
        let alg = if p384 {
            &ECDSA_P384_SHA384_FIXED_SIGNING
        } else {
            &ECDSA_P256_SHA256_FIXED_SIGNING
        };
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
        let k = EcdsaKeyPair::from_pkcs8(alg, pkcs8.as_ref(), &rng).unwrap();
        use ring::signature::KeyPair;
        (
            pem("PRIVATE KEY", pkcs8.as_ref()),
            k.public_key().as_ref().to_vec(),
        )
    }

    fn part(jwt: &str, i: usize) -> Value {
        let p = jwt.split('.').nth(i).unwrap();
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(p).unwrap()).unwrap()
    }

    fn verifies(jwt: &str, alg: &'static dyn VerificationAlgorithm, public: &[u8]) -> bool {
        let (input, sig) = jwt.rsplit_once('.').unwrap();
        UnparsedPublicKey::new(alg, public)
            .verify(input.as_bytes(), &URL_SAFE_NO_PAD.decode(sig).unwrap())
            .is_ok()
    }

    #[test]
    fn each_key_type_signs_an_assertion_with_smarts_claims() {
        let (p384, p384_pub) = ec_pem(true);
        let (p256, p256_pub) = ec_pem(false);
        let rsa = crate::http::google::tests::TEST_KEY;
        let (_, rsa_der) = crate::http::google::pem_block(rsa).unwrap();
        let rsa_pub = RsaKeyPair::from_pkcs8(&rsa_der)
            .unwrap()
            .public()
            .as_ref()
            .to_vec();
        let cases: [(&str, &str, &'static dyn VerificationAlgorithm, &[u8]); 3] = [
            (&p384, "ES384", &ECDSA_P384_SHA384_FIXED, &p384_pub),
            (&p256, "ES256", &ECDSA_P256_SHA256_FIXED, &p256_pub),
            (rsa, "RS384", &RSA_PKCS1_2048_8192_SHA384, &rsa_pub),
        ];
        for (pem, alg, verify, public) in cases {
            let k = ClientKey::from_pem(pem, "key-1").unwrap();
            assert_eq!(k.alg(), alg);
            let jwt = k
                .assertion("my-client", "https://auth.example/token", 1000)
                .unwrap();
            assert!(verifies(&jwt, verify, public), "{alg}");
            assert_eq!(
                part(&jwt, 0),
                json!({"alg": alg, "kid": "key-1", "typ": "JWT"})
            );
            let c = part(&jwt, 1);
            assert_eq!(c["iss"], "my-client");
            assert_eq!(c["sub"], "my-client");
            assert_eq!(c["aud"], "https://auth.example/token");
            assert_eq!(c["iat"], 1000);
            assert_eq!(c["exp"], 1300);
            assert_eq!(c["jti"].as_str().unwrap().len(), 32);
        }
    }

    #[test]
    fn each_assertion_has_its_own_jti() {
        let (pem, _) = ec_pem(true);
        let k = ClientKey::from_pem(&pem, "k").unwrap();
        let a = part(&k.assertion("c", "u", 1).unwrap(), 1);
        let b = part(&k.assertion("c", "u", 1).unwrap(), 1);
        assert_ne!(a["jti"], b["jti"]);
    }

    #[test]
    fn the_form_has_the_grant_scope_and_assertion() {
        let (pem, _) = ec_pem(true);
        let k = ClientKey::from_pem(&pem, "k").unwrap();
        let form = k
            .token_form("c", "https://a/token", Some("system/*.rs"))
            .unwrap();
        let pairs: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(form.as_bytes())
                .into_owned()
                .collect();
        assert_eq!(pairs["grant_type"], "client_credentials");
        assert_eq!(pairs["scope"], "system/*.rs");
        assert_eq!(pairs["client_assertion_type"], ASSERTION_TYPE);
        assert_eq!(pairs["client_assertion"].split('.').count(), 3);
        assert!(!pairs.contains_key("client_secret"));
    }

    #[test]
    fn keys_that_do_not_parse_say_why() {
        let err = |pem: &str| ClientKey::from_pem(pem, "k").unwrap_err();
        assert_eq!(err("nothing"), "not a PEM private key");
        assert!(err(&pem("EC PRIVATE KEY", b"x")).contains("openssl pkcs8"));
        assert!(err(&pem("PRIVATE KEY", b"x")).contains("not an RSA, EC P-384"));
        assert!(err(&pem("PUBLIC KEY", b"x")).contains("not a private key"));
        let (good, _) = ec_pem(true);
        assert_eq!(
            ClientKey::from_pem(&good, "").unwrap_err(),
            "the key id is empty"
        );
    }

    #[test]
    fn a_missing_file_is_a_usage_error_naming_it() {
        let e = ClientKey::read(Path::new("/nonexistent/key.pem"), "k").unwrap_err();
        assert!(matches!(e, PipesError::Usage(_)));
        assert!(e.to_string().contains("/nonexistent/key.pem"), "{e}");
    }
}
