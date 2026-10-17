# Servers and authentication

## Supported servers

| Server | Reading | Authentication | `--server-kind` |
|---|---|---|---|
| Any FHIR R4 server | search; `--bulk` where the server has `$export` | none, bearer token, OAuth client, or HTTP Basic | `generic` (default) |
| HAPI FHIR | search; `--bulk` with `hapi.fhir.bulk_export_enabled=true`; `--hapi-db` reads its PostgreSQL database | whatever is in front of it (often Basic, or OAuth through a gateway such as Keycloak); `--hapi-db` uses `FHIR_PIPES_HAPI_DATABASE_URL` | `generic` or `hapi` (the same) |
| Google Cloud Healthcare API | search; `--bulk` uses the store's `$export` | `--google-auth` | `generic` |
| Medplum | search; `--bulk` | OAuth client: a ClientApplication, `--token-url <server>/oauth2/token` | `medplum` |
| Aidbox | search at `<aidbox>/fhir`; `--bulk` with a storage provider | a Client as Basic (`--username`), or OAuth with `--token-auth basic`, `--token-url <aidbox>/auth/token` | `aidbox` |

Writing (`load`, `sync`) needs transaction Bundles. It takes no kind.

## Auth options

Only one kind at a time. Put secrets in environment variables that the user
sets in their own shell. Never put them on the command line or in a file.

| Kind | What to set |
|---|---|
| Fixed bearer token | `FHIR_PIPES_TOKEN`. It is not renewed; a run that outlasts it fails with 401. |
| OAuth client credentials | `FHIR_PIPES_CLIENT_ID`, `FHIR_PIPES_CLIENT_SECRET`, and `--token-url` (or `FHIR_PIPES_TOKEN_URL`). Optional `--token-auth basic` (`FHIR_PIPES_TOKEN_AUTH`) and `--scope` (`FHIR_PIPES_SCOPE`). The token is renewed a minute before it expires. A wrong secret fails before any FHIR request. |
| HTTP Basic | `--username` (or `FHIR_PIPES_USERNAME`) and `FHIR_PIPES_PASSWORD`. Use HTTPS. |
| Google | `--google-auth` (or `FHIR_PIPES_GOOGLE_AUTH=1`). Credentials come from `GOOGLE_APPLICATION_CREDENTIALS`, then gcloud's application default credentials, then the metadata server. |

`refresh` reads the client id, client secret and password only from the
environment. Its other auth settings can be in `[server]`: `token_url`,
`token_auth`, `scope`, `username`, `google_auth`.

Retries: connection errors, 429 and 5xx are retried `--retries` times
(default 3). The wait follows the server's `Retry-After`, or the `t=` of a
`RateLimit` header on a 429, up to 30 seconds per wait.

## HAPI FHIR

- Use `generic` or `hapi`.
- `--bulk` needs `hapi.fhir.bulk_export_enabled=true`. HAPI does not list
  deletions in a bulk export. After an incremental export, a type whose
  count differs is exported again in full.
- `--hapi-db` reads `hfj_resource`, `hfj_res_ver`, `hfj_res_tag` and
  `hfj_tag_def` (and `hfj_forced_id` where it exists) in one read-only
  transaction. Only PostgreSQL. Tested with HAPI 8.12 on PostgreSQL 17.
  fhir-pipes checks the schema first and refuses one it does not know.
  Use a read-only role:

  ```sql
  CREATE ROLE fhir_pipes_ro LOGIN PASSWORD '...';
  ALTER ROLE fhir_pipes_ro SET default_transaction_read_only = on;
  GRANT CONNECT ON DATABASE hapi TO fhir_pipes_ro;
  GRANT USAGE ON SCHEMA public TO fhir_pipes_ro;
  GRANT SELECT ON hfj_resource, hfj_res_ver, hfj_res_tag, hfj_tag_def TO fhir_pipes_ro;
  ```

- `--hapi-db` is for HAPI only. It does not work with any other server.

## Google Cloud Healthcare API

- `--server projects/<p>/locations/<l>/datasets/<d>/fhirStores/<s>` expands
  to `https://healthcare.googleapis.com/v1/…/fhir`.
- The account needs `roles/healthcare.fhirResourceReader` to read, and
  `roles/healthcare.fhirResourceEditor` for `load` or `sync`.
- `refresh --check` shows where the credentials come from, never the
  credentials themselves.

## Medplum

- Base: `https://api.medplum.com/fhir/R4`, or `<server>/fhir/R4` self-hosted.
  Kind `medplum`.
- Client: a ClientApplication with an AccessPolicy that allows reading each
  type exported. For `--bulk`, the policy must also allow `AsyncJob` and
  `Binary`.
- Token URL: `https://api.medplum.com/oauth2/token` or `<server>/oauth2/token`.
- Without the kind, Medplum refuses `_offset` past 10,000 (HTTP 400
  "Search offset exceeds maximum"). With it, pages use `_cursor` links.
- Counts from 1,000,000 on are estimates. The count check is skipped with a
  warning.
- Rate limits answer 429 with `RateLimit` headers and no `Retry-After`.
  fhir-pipes waits for the `t=` of the spent limit.

## Aidbox

- Base: `<aidbox>/fhir`. The root speaks Aidbox's own format. Kind `aidbox`.
- Client: a `Client` resource with a secret and `grant_types: ["basic"]`,
  and an AccessPolicy for GET. Pass it as `--username` and `FHIR_PIPES_PASSWORD`.
  For client credentials instead: `grant_types: ["client_credentials"]`,
  `--token-url <aidbox>/auth/token --token-auth basic`.
- With the kind, pages ask for `_total=none`. A type over 50,000 resources is
  split into `_lastUpdated` windows.
- `$export` needs `BOX_FHIR_BULK_STORAGE_PROVIDER` (`gcp`, `aws` or `azure`)
  and the matching account resource. Without it the kick-off answers 500
  "storage-type not specified". Aidbox runs one export at a time; fhir-pipes
  waits on its 429 until `--bulk-timeout`.
- Not yet checked against a real Aidbox server.

## A second server (sync)

`sync --server SINK` uses the auth options for the sink. Run `export` against
the source first, then `sync --dry-run`, then `sync`.
