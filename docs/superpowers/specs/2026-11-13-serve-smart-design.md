# SMART on FHIR Backend Services in fhir-pipes

Status: draft for review, 2026-11-13. Issue #161. Builds on
[2026-11-13-serve-token-auth-design.md](2026-11-13-serve-token-auth-design.md)
(`[serve.auth]`).

## Intent

SMART Backend Services is the system-to-system part of SMART App Launch
2.x, also used by the FHIR Bulk Data API. A client gets an access token
from an authorization server with the `client_credentials` grant,
authenticating with a JWT signed by its own private key
(`private_key_jwt`, RFC 7523). It then calls the FHIR API with the token.
There is no user and no sign-in.

`[serve.auth]` already checks the tokens such a server issues. This makes
`serve` and fhir-pipes speak the rest of the profile:

- `serve` says how to get a token, so a FHIR client can find out from
  `serve` itself.
- `serve` understands SMART scopes, such as `system/*.rs`.
- fhir-pipes, as a client in `refresh` and `export`, can get tokens with
  a private key as well as with a client secret.

## Everything is optional

Nothing that works now changes:

- Discovery and the CapabilityStatement's `security` block appear only
  with `[serve.auth]`.
- `query_scope` and `admin_scope` keep their meaning, with no new defaults.
  A SMART deployment sets `query_scope = "system/Library.rs"` itself.
  Without `query_scope`, any valid token from the issuer still opens
  `/fhir`.
- SMART scope matching applies only when the configured scope is itself a
  SMART scope (`system/...`). Any other scope is still matched exactly.
- The client secret keeps working. A private key is another way to send
  the client id.

## Discovery

`GET /fhir/.well-known/smart-configuration`, with `[serve.auth]` and
`[serve.query]`. It needs no token, since a client reads it before it has
one. Without `[serve.auth]` it answers as any unknown `/fhir` path does.

It is built from the provider's OpenID configuration, read at startup:

```json
{
  "issuer": "https://auth.example.org/realms/icr",
  "jwks_uri": "https://auth.example.org/realms/icr/protocol/openid-connect/certs",
  "token_endpoint": "https://auth.example.org/realms/icr/protocol/openid-connect/token",
  "grant_types_supported": ["client_credentials"],
  "token_endpoint_auth_methods_supported": ["private_key_jwt", "client_secret_basic", "client_secret_post"],
  "token_endpoint_auth_signing_alg_values_supported": ["RS384", "ES384"],
  "scopes_supported": ["system/Library.rs", "fhir-pipes.admin"],
  "capabilities": ["client-confidential-asymmetric", "client-confidential-symmetric"]
}
```

- `token_endpoint` comes from the provider. Without one, there is no
  smart-configuration: `serve` logs a warning at startup and answers
  `404`.
- `token_endpoint_auth_methods_supported` and
  `token_endpoint_auth_signing_alg_values_supported` are the provider's.
  Without the first, `["private_key_jwt"]` is assumed; without the second,
  `["RS384", "ES384"]`, the two SMART requires.
- `capabilities` has `client-confidential-asymmetric` when
  `private_key_jwt` is among the methods, and
  `client-confidential-symmetric` when a client-secret method is.
- `scopes_supported` is `query_scope` and `admin_scope`, the ones set.
- `grant_types_supported` is always `["client_credentials"]`: that is the
  only grant that makes sense for `serve`.

## The CapabilityStatement

With `[serve.auth]`, `rest[0].security` is:

```json
{
  "service": [{"coding": [{
    "system": "http://terminology.hl7.org/CodeSystem/restful-security-service",
    "code": "SMART-on-FHIR"}]}],
  "extension": [{
    "url": "http://fhir-registry.smarthealthit.org/StructureDefinition/oauth-uris",
    "extension": [{"url": "token", "valueUri": "<token_endpoint>"}]}],
  "description": "SMART Backend Services: send an access token from <issuer>. See /fhir/.well-known/smart-configuration."
}
```

The `oauth-uris` extension is older SMART, still read by some clients.
`/fhir/metadata` still needs a token, as now.

## SMART scopes

A SMART scope is `<context>/<resource>.<permissions>`:
- the context is `patient`, `user` or `system`;
- the resource is a resource type or `*`;
- the permissions are SMART v2's `c`, `r`, `u`, `d` and `s`, in that order,
  or SMART v1's `read`, `write` or `*`.

When `query_scope` or `admin_scope` is a SMART scope, a token's scope
grants it when:
- it has the same context;
- its resource is the same, or `*`;
- its permissions include every permission the configured scope has.
  SMART v1 `read` counts as `rs`, `write` as `cud` and `*` as `cruds`.

So with `query_scope = "system/Library.rs"`, a token with
`system/Library.rs`, `system/Library.cruds`, `system/*.rs`,
`system/Library.read` or `system/*.read` opens `/fhir`. A token with
`system/Library.r`, `patient/Library.rs` or `system/Patient.rs` does not.

A token's scope with SMART v2's query part, for example
`system/Observation.rs?category=laboratory`, is narrower than the scope
without one. It never grants a configured scope that has no query part.

## fhir-pipes as a client: `private_key_jwt`

`refresh`, `export` and the other FHIR commands get a token with a private
key instead of a client secret:

| Flag | Variable | `[server]` key | |
|---|---|---|---|
| `--client-id` | `FHIR_PIPES_CLIENT_ID` | | as now |
| `--client-key` | `FHIR_PIPES_CLIENT_KEY` | `client_key` | path to a PEM private key: RSA (PKCS #8 or #1) or EC P-384 or P-256 (PKCS #8) |
| `--client-key-id` | `FHIR_PIPES_CLIENT_KEY_ID` | `client_key_id` | the key's `kid`, as registered with the provider |
| `--token-url` | `FHIR_PIPES_TOKEN_URL` | `token_url` | as now |
| `--scope` | `FHIR_PIPES_SCOPE` | `scope` | as now; SMART servers want one, such as `system/*.rs` |

- A client id takes a client secret or a client key, not both. A client
  key needs its key id.
- In `[server]`, `client_key` is relative to the config file.
- For each token, fhir-pipes signs a new assertion:
  - header: `alg` (RS384 for an RSA key, ES384 for P-384, ES256 for P-256),
    `kid` and `typ: JWT`;
  - claims: `iss` and `sub` are the client id, `aud` is the token URL,
    `exp` is 5 minutes ahead, plus `iat` and a random `jti`.

  It posts `grant_type=client_credentials`, the `scope`,
  `client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer`
  and `client_assertion`.
- A key file that does not parse stops the command with a usage error
  naming the file, never quoting it. An `EC PRIVATE KEY` (SEC1) file gets
  a hint: `openssl pkcs8 -topk8 -nocrypt -in key.pem -out key-pkcs8.pem`.

The key is held only in memory and never logged. Its path is not secret.

## Testing

- **Scopes:** units for SMART v2 and v1 forms, wildcards, contexts, query
  parts, and non-SMART scopes matched exactly.
- **Discovery:** integration tests that `smart-configuration` needs no
  token, has what the provider's configuration says, and is absent without
  `[serve.auth]`. Also the CapabilityStatement's `security`.
- **Client:** an `httptest` token endpoint that checks the assertion's
  signature with the public key, and its claims and form. Run with an RSA
  key and with an EC key.
- **End to end:** fhir-pipes as a `private_key_jwt` client gets a token
  from a mock provider and calls a `serve` that checks it.

## Not included

- Registering clients or publishing a client's JWKS (`jwks_uri` for a
  client): that is the provider's.
- SMART App Launch, the user-facing part: authorization codes, launch
  contexts, sign-in.
- Patient or user scopes doing more than matching: `serve` has no patients.
