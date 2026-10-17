# Servers

fhir-pipes reads any FHIR R4 server through its API: paged searches with
`_lastUpdated` and `Bundle.link`, and the system-level Bulk Data `$export`
with `--bulk`. Some servers page or count in ways that need other search
parameters. `--server-kind` (`server.kind` in `refresh`) names the server so
that `export`, `extract` and `run` send them:

| Kind | What changes |
|---|---|
| `generic` (default) | Nothing. |
| `hapi` | Nothing: HAPI FHIR searches as `generic` does. |
| `medplum` | Every page asks for `_sort=_lastUpdated`. A count from 1,000,000 on is taken as an estimate. `_lastUpdated` windows are halved until they hold fewer than 1,000,000 resources. With `--bulk`, `Binary/<id>` output files are read from the FHIR base with the token. |
| `aidbox` | Every page asks for `_total=none`. `_lastUpdated` windows are halved until they hold at most 50,000 resources, and a larger type is paged in windows even with `--concurrency 1`. |

The kind's parameters are added to the first page of each search and kept on
the server's next links. The kind does not change authentication or
`--hapi-db`.

The window limits are targets, not guarantees. The windows' counts
must add up to the type's before any halving, and past the usual number of
windows (eight per `--concurrency`), halving stops at a window whose halves
do not count up to it. A server whose window counts do not add up is paged
as one stream.

```sh
fhir-pipes export --server https://api.medplum.com/fhir/R4 --server-kind medplum \
  --client-id $MEDPLUM_CLIENT_ID --token-url https://api.medplum.com/oauth2/token \
  --type Patient --type Observation --out raw/
```

The database route (`--hapi-db`) is for HAPI only. Medplum and Aidbox are read
through the FHIR API.

## HAPI FHIR

Use `generic` or `hapi`; they are the same. `export --bulk` needs
`hapi.fhir.bulk_export_enabled=true`. HAPI does not list deletions in a bulk
export, so a type whose count differs after an incremental export is exported
again in full. See also
[Reading a HAPI server's database](../README.md#reading-a-hapi-servers-database).

## Medplum

The FHIR base is `https://api.medplum.com/fhir/R4` for the hosted service, or
`<server>/fhir/R4` for a self-hosted one. Use `--server-kind medplum`.

**Client.** Create a ClientApplication in the project (Project admin,
Clients), and give it an AccessPolicy that allows reading each type you
export. For `--bulk`, the policy must also allow reading `AsyncJob` (the
export's status) and `Binary` (the output files). For example:

```json
{
  "resourceType": "AccessPolicy",
  "name": "fhir-pipes export",
  "resource": [
    {"resourceType": "Patient", "readonly": true},
    {"resourceType": "Observation", "readonly": true},
    {"resourceType": "AsyncJob", "readonly": true},
    {"resourceType": "Binary", "readonly": true}
  ]
}
```

Pass the ClientApplication's id and secret with `--client-id` and
`--client-secret` (or `FHIR_PIPES_CLIENT_ID` and `FHIR_PIPES_CLIENT_SECRET`),
and `--token-url https://api.medplum.com/oauth2/token` (or
`<server>/oauth2/token`). fhir-pipes gets its token with the
client-credentials grant.

**Paging.** Medplum pages by `_offset` and refuses an offset over 10,000
(HTTP 400, "Search offset exceeds maximum"), so without the kind a type stops
at about 11,000 resources. With `_sort=_lastUpdated` and a `_count` of 20 or
more, Medplum gives `_cursor` next links instead, which have no limit.

**Counts.** Medplum counts exactly below its `accurateCountThreshold`
(1,000,000 by default; a self-hosted server can change it) and estimates from
there on. fhir-pipes does not compare an estimate with the file: the count
check after an incremental search is skipped, with a warning, so a resource
deleted from such a type stays in the file until a full download. Windowed
downloads halve each window until it holds fewer than 1,000,000, so its count
is exact. A window is never cut below one second, so a second holding a
million or more stays one window, with an estimated count.

**Bulk export.** A system-level `$export` covers the whole project. The
output files are listed as `Binary/<id>`, relative to the FHIR base, and need
the access token even though the manifest says `requiresAccessToken: false`;
with `--server-kind medplum` fhir-pipes reads them that way. An export with
`_since` does not list deleted resources, so the count check after the merge
finds the difference and the type is exported again in full.

**Rate limits.** Medplum limits requests per IP address per minute (6,000 on
the hosted free plan, 60,000 on paid plans and by default when self-hosted),
and FHIR interactions per user per minute (50,000 points by default; a search
costs 20 points, so about 2,500 searches a minute). A self-hosted server sets
them with `defaultRateLimit` and `defaultFhirQuota`
(`MEDPLUM_DEFAULT_RATE_LIMIT`, `MEDPLUM_DEFAULT_FHIR_QUOTA`). A 429 answer
carries `RateLimit` headers (`"fhirInteractions";r=0;t=45`) and no
`Retry-After`.

Checked against a self-hosted Medplum 5.2.5: search of 12,499 Observations as
one stream (13 pages) and in 12 windows, incremental search, and bulk export
in full and with `_since`.

## Aidbox

The FHIR base is `<aidbox>/fhir`. The root (`<aidbox>/Patient`) answers in
Aidbox's own format, which fhir-pipes does not read. Use
`--server-kind aidbox`.

**Client.** Create a Client resource with a secret and the `basic` grant type,
and an AccessPolicy that lets it read:

```json
{
  "resourceType": "Client",
  "id": "fhir-pipes",
  "secret": "<secret>",
  "grant_types": ["basic"]
}
```

```json
{
  "resourceType": "AccessPolicy",
  "id": "fhir-pipes-read",
  "engine": "matcho",
  "matcho": {"client": {"id": "fhir-pipes"}, "request-method": "get"}
}
```

Pass the client id and secret with `--username` and `--password` (or
`FHIR_PIPES_USERNAME` and `FHIR_PIPES_PASSWORD`). For the client-credentials
grant instead (`grant_types: ["client_credentials"]`, token URL
`<aidbox>/auth/token`), Aidbox documents only the request with the id and
secret in a Basic header, not in the form body that fhir-pipes sends today.

**Paging.** By default Aidbox counts the whole result on every page
(`_total=accurate`), which is slow on a large type and counts against its
search timeout. With the kind, every page asks for `_total=none`. Aidbox pages
with an offset (`_page`), which slows down deep into a large result, so a
type of more than 50,000 resources is split into `_lastUpdated` windows, even
with `--concurrency 1`, and each window holding more than 50,000 is halved.
50,000 is a target, not a guarantee: a window is never cut below one second,
so a second holding more than 50,000 resources stays one window.

**Bulk export.** `$export` needs a storage provider for its files:
`BOX_FHIR_BULK_STORAGE_PROVIDER` set to `gcp`, `aws` or `azure`, with the
matching account resource (for example `AwsAccount`, which also works with
MinIO). Without it the kick-off answers 500 "storage-type not specified", and
fhir-pipes says which setting is missing. The output files are signed URLs,
downloaded without the token. Aidbox runs one export at a time and answers a
second kick-off with 429.

Not yet checked against a real Aidbox server, which needs a licence: the
largest `_count`, `_sort=_lastUpdated` (windows need it; without it the type
is paged as one stream), `Retry-After` and `X-Progress` on the status URL,
what `transactionTime` means, and whether deleted resources are listed.

## Tests against real servers

`cargo test --test export_servers` runs the fakes, and also a real server
when the environment names one:

| Server | Environment |
|---|---|
| Medplum | `FHIR_PIPES_TEST_MEDPLUM_URL` (the FHIR base), `FHIR_PIPES_TEST_MEDPLUM_CLIENT_ID`, `FHIR_PIPES_TEST_MEDPLUM_CLIENT_SECRET`; `FHIR_PIPES_TEST_MEDPLUM_TOKEN_URL` if not `/oauth2/token` on the same host |
| Aidbox | `FHIR_PIPES_TEST_AIDBOX_URL` (the `/fhir` base), `FHIR_PIPES_TEST_AIDBOX_USERNAME`, `FHIR_PIPES_TEST_AIDBOX_PASSWORD`; `FHIR_PIPES_TEST_AIDBOX_BULK=1` to check `$export` too |

`FHIR_PIPES_TEST_MEDPLUM_TYPES` and `FHIR_PIPES_TEST_AIDBOX_TYPES`
(comma-separated, default `Patient`) name the types downloaded. Each test
exports by search twice (the second run must not download in full) and, for
Medplum, by `$export`, whose files must hold the same ids.

A local Medplum for these tests runs from the `medplum/medplum-server` image
with PostgreSQL and Redis (Apache-2.0), started with
`node packages/server/dist/index.js env` and the `MEDPLUM_*` settings from
Medplum's self-hosting guide.
