# fhir-pipes serve

`fhir-pipes serve` runs refreshes for another system: it starts a refresh when
asked over HTTP or at a scheduled time, keeps a record of every run, and tells
configured destinations when a run ends. With `[serve.query]` it also answers
`$sql-run` for approved SQLQuery Libraries. It is for Unix (Linux, macOS), not
the Windows binary.

## Running it

```sh
export FHIR_PIPES_SERVE_TOKEN='a long random string'
fhir-pipes serve --config fhir-pipes.toml --listen 0.0.0.0:8090
```

It reads the same `fhir-pipes.toml` as `refresh`, plus a `[serve]` section.
`--listen` (or `FHIR_PIPES_SERVE_LISTEN`) defaults to `127.0.0.1:8090`. Callers
send `Authorization: Bearer <token>`. Without `FHIR_PIPES_SERVE_TOKEN`, `serve`
refuses to start on a non-loopback address.

One `serve` looks after one warehouse and holds `.serve/serve.lock` in it. Run
records are in `.serve/runs/<run id>/`. Do not also run `refresh` from cron on
it.

### In Docker Compose

```yaml
  fhir-pipes:
    command: ["serve", "--config", "/config/fhir-pipes.toml", "--listen", "0.0.0.0:8090"]
    environment:
      FHIR_PIPES_SERVE_TOKEN: ${FHIR_PIPES_SERVE_TOKEN}
    volumes:
      - ./fhir-pipes:/config:ro
      - warehouse:/warehouse
    init: true
    restart: unless-stopped
    stop_grace_period: 150s      # more than stop_grace
```

## The config
```toml
[serve]
retries = 2               # attempts after the first
retry_delay = 600         # seconds between attempts
stop_grace = 120          # seconds a stopping refresh gets before SIGKILL
keep_runs = 100           # finished runs kept

[[serve.schedule]]
cron = "0 */6 * * *"      # five fields, UTC
mode = "incremental"

[[serve.notify]]
name = "openfn"
url = "https://openfn.example.org/i/2b1c…"
header = "x-api-key"              # optional, with value_env
value_env = "OPENFN_WEBHOOK_KEY"  # the variable holding the header's value
```

The values shown are the defaults. A notify `url` must be `http` or `https`
with no user name or password; put a key in `header` and `value_env`.

## The API

Every path but `/health` needs the token.

| Request | What it does |
|---|---|
| `POST /run` | Start a run. Optional body: `{"mode": "incremental" \| "full", "only": [steps], "skip": [steps], "push": true}`. `202` with `{"id", "status_url"}`; `409` if a run is under way. |
| `GET /status` | The current run, the last finished run and the next scheduled time |
| `GET /runs?limit=50`, `GET /runs/{id}` | Recent runs, newest first; one run's record |
| `GET /runs/{id}/log?tail=200` | A run's output as text |
| `POST /runs/{id}/cancel` | Stop a run; `409` once it has ended |
| `GET /metrics`, `GET /health` | Prometheus metrics; `ok` |

The steps are `refresh`'s: `locations`, `views`, `tiles`, `libraries`,
`manifest`, `catalog`, `database`, `hooks`, `push`. A run's `status` is
`running`, `waiting`, `succeeded`, `failed`, `cancelled` or `interrupted`. Each attempt's `result` is the refresh's `last-run.json` (see
[refresh.md](refresh.md#how-the-last-run-went)).

## Retries, notify and the schedule

- **Retries.** A failed attempt is tried again after `retry_delay`, up to
  `retries` times, only when it failed before the database step (at `lock`,
  `tools`, `start`, `files`, `locations`, `views`, `tiles`, `manifest` or
  `catalog`). Failures in the database step, publish, hooks or push, config
  errors, signals from outside `serve` and cancelled runs are not retried.
- **Notify.** When a run ends, `serve` posts a JSON summary (`event_id`,
  `run_id`, `status`, `attempts`, `result`) to each `[[serve.notify]]`
  destination. Delivery is at least once: ignore an `event_id` already seen.
- **Alerts.** Refreshes under `serve` run with failure hooks off, since each
  retry would run them. Use a notify destination for failure alerts.
- **Schedule.** Each `[[serve.schedule]]` starts a run at its cron times (UTC).
  A time that comes while a run is under way, or while `serve` is stopped, is
  skipped.

On SIGTERM, `serve` stops the running refresh as
[refresh.md](refresh.md#stopping-a-run) describes, with SIGKILL after `stop_grace`.

## Queries ($sql-run)

`serve` can run the warehouse's SQLQuery Libraries for a caller and return the
rows, as the SQL on FHIR `$sql-run` operation does. The API is off unless the
config has a `[serve.query]` section. Only approved SQLQuery Libraries in the
catalog run, and a caller cannot send SQL.

```toml
[serve.query]
approve = "active"        # "active", "listed" or "all"
# queries = ["https://example.org/Library/CoverageByArea"]   # with "listed"
source = "local"          # "local", or a prefix DuckDB can read, such as "r2://bucket/warehouse"
# duckdb_setup = "query-setup.sql"   # SQL run before each query
timeout = 60              # seconds each duckdb run may take
max_rows = 100_000        # most rows one query returns
concurrency = 4           # queries that may run at once
```

The values shown are the defaults. `queries` goes with `approve = "listed"`
only. A mistake in `[serve]` or `[serve.query]` stops `serve` with exit 2.

| `approve` | Runs |
|---|---|
| `active` | Every SQLQuery whose `status` is `active`. A `draft` does not run. |
| `listed` | The SQLQuery Libraries in `queries`, each as its url or `url\|version`. |
| `all` | Every SQLQuery, drafts too. |

A SQLView or a ViewDefinition never runs. An approved query's SQL runs in
DuckDB with `serve`'s access to the file system and the network, so approving
a query is a decision to trust it.

### Tokens

`FHIR_PIPES_QUERY_TOKEN` opens `/fhir/*` only. `FHIR_PIPES_SERVE_TOKEN` opens
everything. Callers send either as `Authorization: Bearer <token>`. With
`[serve.query]`, a non-loopback `--listen` needs both. A path with no token set
is open, allowed only on loopback: setting only `FHIR_PIPES_QUERY_TOKEN` leaves
the run API open to anything on the machine, so set both. Without
`[serve.query]`, every `/fhir` path answers `404`.

### Routes and parameters

| Request | What it does |
|---|---|
| `GET /fhir/$sql-run` | Run a query; parameters in the query string |
| `POST /fhir/$sql-run` | The same, with a Parameters resource as the body (at most 64 KB) |
| `GET /fhir/metadata`, `GET /fhir/OperationDefinition/fhir-pipes-sql-run` | CapabilityStatement and OperationDefinition |
| `GET /fhir/Library` | A Bundle of the approved SQLQuery Libraries, each with its SQL |

| Parameter | Type in a Parameters body | |
|---|---|---|
| `subjectCanonical` | `valueCanonical` | Required. The Library's url, or `url\|version`. |
| `_format` | `valueCode` | `ndjson`, `json` or `csv`. Otherwise the `Accept` header decides; the default is NDJSON. |
| `_limit` | `valueInteger` | The most rows to return, at least 1. |

Any other parameter is `400`. A result cut at `max_rows` has the header
`X-Fhir-Pipes-Truncated: max_rows=<n>`.

```sh
curl -H "Authorization: Bearer $FHIR_PIPES_QUERY_TOKEN" -G \
  'http://localhost:8090/fhir/$sql-run' \
  --data-urlencode 'subjectCanonical=https://example.org/Library/CoverageByArea' \
  --data-urlencode '_format=csv'

curl -H "Authorization: Bearer $FHIR_PIPES_QUERY_TOKEN" -H 'Content-Type: application/fhir+json' \
  'http://localhost:8090/fhir/$sql-run' -d '{"resourceType":"Parameters","parameter":[
    {"name":"subjectCanonical","valueCanonical":"https://example.org/Library/CoverageByArea"}]}'
```

### Statuses

`200` returns the rows. Error bodies are OperationOutcomes, except `401` and the
`404` of a disabled API.

| Status | When |
|---|---|
| `400` | A missing, repeated or unknown parameter; a bad `_format` or `_limit`; a body that is not a Parameters resource. |
| `401` | No token, or a wrong one. |
| `403` | The Library is in the catalog but is not an approved SQLQuery. |
| `404` | The canonical is not in the catalog, or a url alone matches more than one version. |
| `413`, `415` | A POST body over 64 KB; a `Content-Type` other than `application/fhir+json` or `application/json`. |
| `422` | The query or the `duckdb_setup` SQL failed, or ran past `timeout`. |
| `503` | An input table has no data yet, a refresh is publishing, `concurrency` queries are running, or `serve` is stopping. `Retry-After: 5`. |

### Limits

- Callers see fixed error text, never DuckDB's message; the details are in
  `serve`'s log.
- With `source = "local"`, a query overlapped twice by a refresh's publish gets
  `503`. With a remote `source`, a query that runs while `rclone` pushes a
  refresh can read a mix of old and new files.
- A Library's `[[parameter]]` entries are not supported yet.
