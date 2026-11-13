# Access tokens from an identity provider in `fhir-pipes serve`

Status: draft for review, 2026-11-13.

## Intent

`serve` now accepts two shared tokens, `FHIR_PIPES_SERVE_TOKEN` and
`FHIR_PIPES_QUERY_TOKEN`, compared byte for byte. Everyone given the query
token shares it: it does not expire, it cannot be withdrawn from one caller,
and the log cannot say who called.

This adds an optional `[serve.auth]` section. With it, `serve` also accepts
OAuth 2.0 access tokens that are JWTs signed by an identity provider the
deployment already runs (Keycloak, Microsoft Entra ID, Auth0, Okta, Google
and others). Each caller gets its own client from the provider and asks it
for a short-lived token; `serve` checks the token and never sees a secret.

It is an API for programs. There is no sign-in page and no browser flow.

## Goals

- Accept JWT access tokens from one issuer, checked against the issuer's
  published signing keys, with no shared secret between the provider and
  `serve`.
- Work with any provider that publishes OpenID Connect discovery, set up
  from the issuer URL alone.
- Let a scope (or role) decide whether a token opens the query API, the
  run API, or both.
- Keep the shared tokens working beside it. A deployment without
  `[serve.auth]` behaves exactly as before.

## Non-goals for now

- **SMART on FHIR Backend Services** (see the last section). This design
  checks the same tokens SMART Backend Services issues, so it is the first
  half of that work.
- Opaque (non-JWT) access tokens, which need token introspection
  (RFC 7662).
- More than one issuer.
- A list of queries per caller, or rows filtered by who is calling.
- Issuing tokens or registering clients: that is the provider's job.
- The client side: `refresh` and `export` getting tokens with a signed JWT
  (`private_key_jwt`) rather than a client secret.

## Anti-goals

- An unsigned token (`alg: none`) or one signed with a shared-secret
  algorithm (`HS256` and so on) must never be accepted.
- A token for another service (another audience) must never open `serve`.
- A token must never be written to the log.

## The config

```toml
[serve.auth]
issuer = "https://auth.example.org/realms/icr"   # required
audience = "fhir-pipes"                          # required: the token's aud must include it
query_scope = "fhir-pipes.query"                 # optional: needed to open /fhir
admin_scope = "fhir-pipes.admin"                 # optional: opens the run API as well
```

- `issuer` is the provider's issuer URL, exactly as its tokens' `iss`
  claim has it. It must be `https`, except on a loopback host (for tests).
- `audience` is required: without it, any token from the same provider,
  meant for any service, would open `serve`.
- `query_scope`: without it, any valid token from the issuer for the
  audience opens `/fhir`. With it, the token must carry that scope, or
  `admin_scope`.
- `admin_scope`: a token carrying it opens every path, as the serve token
  does. Without it, a token never opens the run API; only
  `FHIR_PIPES_SERVE_TOKEN` does.
- Any other key is an error, and a mistake stops `serve` with exit 2, as
  for the rest of `[serve]`.

### Where a scope is looked for

Providers put scopes and roles in different claims. A scope matches when it
is in any of:

| Claim | Form | Used by |
|---|---|---|
| `scope` | space-separated string | Keycloak, Auth0, Okta, RFC 9068 |
| `scp` | string or array | Microsoft Entra ID (delegated), Okta |
| `roles` | array | Microsoft Entra ID app roles (client credentials) |

## Which credential opens which path

| Path | Opened by |
|---|---|
| `/health` | anyone |
| `/fhir/*` | the query token, the serve token, or a JWT with `query_scope` (any JWT if unset) or `admin_scope` |
| everything else | the serve token, or a JWT with `admin_scope` |

A path is open with no credential only when none of the credentials that
could open it is configured, and then `serve` must listen on loopback, as
now. For a non-loopback `--listen`:

- the run API needs `FHIR_PIPES_SERVE_TOKEN` or `admin_scope`;
- with `[serve.query]`, `/fhir` needs `FHIR_PIPES_QUERY_TOKEN` or
  `[serve.auth]`.

## Checking a token

A bearer value is compared with the shared tokens first, in constant time.
If none matches and `[serve.auth]` is set, it is checked as a JWT:

1. Three base64url parts. The header's `alg` must be one of `RS256`,
   `RS384`, `RS512`, `PS256`, `PS384`, `PS512`, `ES256`, `ES384` or
   `EdDSA` (Ed25519). Anything else is refused before any key is looked up.
2. The key is the issuer's key with the header's `kid`. A token without a
   `kid` is checked against the one key of a matching type, if there is
   exactly one. A key whose `use` is not `sig`, or whose `alg` differs
   from the header's, is not used.
3. The signature is checked with `ring`, which fhir-pipes already uses to
   sign Google service-account assertions. No new crypto dependency.
4. Claims: `iss` equals `issuer`; `aud` (a string or an array) includes
   `audience`; `exp` is present and in the future; `nbf`, when present, is
   not in the future. 60 seconds of clock skew are allowed.
5. The scope the path needs, as above.

### Keys

- At startup `serve` reads `<issuer>/.well-known/openid-configuration`.
  Its `issuer` must equal the configured one, which catches a missing or
  extra trailing slash. It then reads the document's `jwks_uri`.
- If either fails, `serve` stops with exit 2 and says which. A wrong issuer
  is found at once instead of at the first call.
- A token whose `kid` is not among the keys makes `serve` read the keys
  again, at most once a minute, so a provider's new key works without a
  restart. Keys read more than an hour ago are read again at the next
  call, so a key the provider withdraws stops working.
- Reading keys runs off the async threads, with a 10-second timeout and a
  1 MB limit.

### Responses

| When | Status | `WWW-Authenticate` |
|---|---|---|
| No token, or a token that is not valid | `401` | `Bearer realm="fhir-pipes"`, with `error="invalid_token"` when a token was sent |
| A valid token without the scope the path needs | `403` | `Bearer realm="fhir-pipes", error="insufficient_scope", scope="<needed>"` |

The body says nothing about why a token was refused. `serve`'s log does,
without the token: for example `auth: token refused: expired (client
dashboards-export)`, using `azp` or `client_id` when the token has one.

## Testing

- Unit tests sign tokens with ring-generated P-256 and Ed25519 keys and the
  RSA test key already in `tests/fixtures/google`. They cover each
  algorithm, the claim checks, `alg: none` and `HS256` refused, a key used
  for the wrong algorithm, scopes from each claim, and reading the keys
  again for an unknown `kid` at most once a minute.
- Integration tests run `serve` against an `httptest` provider (discovery
  and keys) and check each row of the path table, the 401 and 403 headers,
  the non-loopback rules, and that a token is never in the log.
- By hand: a Keycloak client with the client-credentials grant and an
  audience mapper.

## Later: SMART on FHIR Backend Services

SMART Backend Services is the system-to-system part of SMART App Launch
2.x, used by the FHIR Bulk Data API: a client gets an access token with the
`client_credentials` grant, authenticating with a JWT signed by its own
private key (`private_key_jwt`), and calls the FHIR API with it. `serve`
would be the resource server, as above. Adding it means:

1. **Discovery.** `GET /fhir/.well-known/smart-configuration`, built from
   the provider's OpenID configuration: `token_endpoint`,
   `grant_types_supported: ["client_credentials"]`,
   `token_endpoint_auth_methods_supported: ["private_key_jwt"]`,
   `token_endpoint_auth_signing_alg_values_supported`, `scopes_supported`
   and `capabilities: ["client-confidential-asymmetric"]`. The
   CapabilityStatement gets a `rest.security` with the `SMART-on-FHIR`
   service code. FHIR clients can then find out how to get a token from
   `serve` itself.
2. **SMART scopes.** SMART v2 scopes name resource types, not operations.
   `system/Library.rs` (read and search Library) fits `/fhir/Library` and
   `$sql-run` best, since the queries are Libraries. Defaults would be
   `query_scope = "system/Library.rs"`, with a custom admin scope for the
   run API, still configurable. `system/*.rs` and SMART v1's
   `system/Library.read` would also be accepted.
3. **The client side.** `refresh` and `export` get tokens with
   `private_key_jwt` from a key file and key id, so fhir-pipes can call
   SMART Backend Services servers too, including another `serve`. The JWT
   signing is the code Google service accounts already use.
4. **Docs.** How to register a backend-services client in Keycloak, with
   its public key or a JWKS URL.
