# Servers

fhir-pipes reads any FHIR R4 server through its API: paged searches with
`_lastUpdated` and `Bundle.link`, and the system-level Bulk Data `$export` with
`--bulk` (in HAPI, `hapi.fhir.bulk_export_enabled=true`). Tested: HAPI FHIR, Medplum,
Aidbox and the Google Cloud Healthcare API.

## Connecting to a server

`--server` is the FHIR base URL. It cannot carry a user name or password; use
the options below. FHIR request credentials stay on the `--server` origin, except that authenticated bulk output URLs supplied by the server are also sent them.

| Auth | Flags | Environment variables |
|---|---|---|
| Bearer token | `--token` | `FHIR_PIPES_TOKEN` |
| OAuth client credentials | `--client-id`, `--client-secret`, `--token-url`, `--token-auth post\|basic`, `--scope` | `FHIR_PIPES_CLIENT_ID`, `FHIR_PIPES_CLIENT_SECRET`, `FHIR_PIPES_TOKEN_URL`, `FHIR_PIPES_TOKEN_AUTH`, `FHIR_PIPES_SCOPE` |
| Username and password (HTTP Basic) | `--username`, `--password` | `FHIR_PIPES_USERNAME`, `FHIR_PIPES_PASSWORD` |
| Google Application Default Credentials | `--google-auth` | `FHIR_PIPES_GOOGLE_AUTH=1` |

- A fixed token is not renewed: a longer run fails with a 401. Basic auth needs HTTPS.
- Client credentials are renewed a minute before expiry. `--token-auth post`
  (default) sends the client in the form body; `basic` in a Basic header.
  `--scope` adds a `scope`, such as `system/*.read`.
- `refresh` reads the client secret and password only from the environment.

`--server-kind` (in `refresh` also `FHIR_PIPES_SERVER_KIND` or `server.kind`)
adds the search parameters some servers need:

| Kind | What changes |
|---|---|
| `generic` (default), `hapi` | Nothing. |
| `medplum` | Pages ask for `_sort=_lastUpdated`; large types are split into `_lastUpdated` windows. With `--bulk`, `Binary/<id>` output files are read from the FHIR base with the token. |
| `aidbox` | Pages ask for `_total=none`; types over 50,000 resources are split into `_lastUpdated` windows. |

A next link to another origin stops the run; behind a proxy, HAPI needs
`X-Forwarded-Host` and `X-Forwarded-Proto`. Files `export` and `extract` write
are mode 0600 on Unix, since they can hold patient data.

## HAPI FHIR

Use `generic` or `hapi`. HAPI does not list deletions in a bulk export, so a
type whose count differs after an incremental export is exported again in full.
`--hapi-db` reads HAPI's database directly (HAPI only).
## Google Cloud Healthcare API

Use `generic` and `--google-auth`. `--server` is the store URL or the store
path `projects/<p>/locations/<l>/datasets/<d>/fhirStores/<s>`.

Credentials are found in Google's usual order: the key file named by
`GOOGLE_APPLICATION_CREDENTIALS`, then gcloud's application default credentials,
then the metadata server on Compute Engine, Cloud Run and GKE. The account needs
`roles/healthcare.fhirResourceReader` to read and
`roles/healthcare.fhirResourceEditor` to write (`load`, `sync`).

```sh
export GOOGLE_APPLICATION_CREDENTIALS=$HOME/keys/fhir-reader.json
fhir-pipes export --google-auth \
  --server projects/my-project/locations/us-central1/datasets/health/fhirStores/main \
  --type Patient --type Observation --out raw/
```

## Medplum

The FHIR base is `https://api.medplum.com/fhir/R4`, or `<server>/fhir/R4` when
self-hosted. Use `--server-kind medplum`.

- Create a ClientApplication whose AccessPolicy allows reading each exported
  type (and `AsyncJob` and `Binary` for `--bulk`). Use its id and secret as
  client credentials with `--token-url <server>/oauth2/token`.
- Medplum estimates counts from 1,000,000 resources, so the count check after an
  incremental search is skipped with a warning.

## Aidbox

The FHIR base is `<aidbox>/fhir`. Use `--server-kind aidbox`.

- Create a Client with a secret and the `basic` grant type, and an AccessPolicy
  that lets it read. Pass the id and secret as `--username` and `--password`.
  For the client-credentials grant instead, use `--token-url <aidbox>/auth/token`
  and `--token-auth basic`.
- `$export` needs a storage provider: set `BOX_FHIR_BULK_STORAGE_PROVIDER` to
  `gcp`, `aws` or `azure` with the matching account resource.

## Tests against real servers

`cargo test --test export_servers` runs the fakes, and a real server when
`FHIR_PIPES_TEST_MEDPLUM_URL` (with `..._CLIENT_ID`, `..._CLIENT_SECRET`) or
`FHIR_PIPES_TEST_AIDBOX_URL` (with `..._USERNAME`, `..._PASSWORD`,
optionally `..._BULK=1`) is set. `FHIR_PIPES_TEST_MEDPLUM_TYPES` and `FHIR_PIPES_TEST_AIDBOX_TYPES`
name the types downloaded (default `Patient`).
