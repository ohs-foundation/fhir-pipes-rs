# fhir-pipes serve

`fhir-pipes serve` runs refreshes for another system. It starts a refresh
when asked over HTTP or at a scheduled time, keeps a record of every run,
and tells configured destinations when a run ends. A typical use: an OpenFn
workflow loads data into HAPI, then asks `serve` to refresh the warehouse;
when the refresh ends, `serve` posts the result to an OpenFn webhook
trigger, which carries on (pushing aggregates to DHIS2, say) or raises an
alert.

`serve` is built for Unix: Linux, in Docker, at the sites, and macOS for
development. It is not in the Windows binary.

The design, and why it is this way, is in
[docs/superpowers/specs/2026-10-21-serve-design.md](superpowers/specs/2026-10-21-serve-design.md).

## Running it

```sh
export FHIR_PIPES_SERVE_TOKEN='a long random string'
fhir-pipes serve --config fhir-pipes.toml --listen 0.0.0.0:8090
```

It reads the same `fhir-pipes.toml` as `refresh`, with a `[serve]` section
that `refresh` ignores. `--listen` (or `FHIR_PIPES_SERVE_LISTEN`) is
`127.0.0.1:8090` by default. Callers send the token as
`Authorization: Bearer <token>`. Without `FHIR_PIPES_SERVE_TOKEN`, `serve`
listens only on a loopback address and refuses to start on any other.

One `serve` looks after one warehouse. It holds `.serve/serve.lock` in the
warehouse directory while it runs, and a second `serve` on the same
warehouse fails at start. Run records and logs are kept in
`.serve/runs/<run id>/`.

Each run is a child `fhir-pipes refresh` in a process group of its own, so
`serve` can stop it and everything it started. It is given the warehouse
`serve` holds with `--data`, whichever of `--data`, `FHIR_PIPES_DATA` and the
config named it. Its result file, which the refresh writes for `serve`, is
removed once read; the run's record keeps a copy with the secrets hidden. A run started by `serve`
leaves the same warehouse as the same refresh run from cron. Do not also
run `refresh` from cron on a warehouse `serve` looks after: the two would
take turns on the warehouse's lock, and a run of `serve`'s that finds it
held fails at `lock` (and is retried).

### In Docker Compose, beside HAPI and the Info Gateway

```yaml
  fhir-pipes:
    image: fhir-pipes            # a small image with the fhir-pipes binary
    command: ["serve", "--config", "/config/fhir-pipes.toml", "--listen", "0.0.0.0:8090"]
    environment:
      FHIR_PIPES_SERVE_TOKEN: ${FHIR_PIPES_SERVE_TOKEN}
      FHIR_PIPES_CLIENT_ID: fhir-pipes           # a Keycloak client the Info Gateway accepts
      FHIR_PIPES_CLIENT_SECRET: ${FHIR_PIPES_CLIENT_SECRET}
      OPENFN_WEBHOOK_KEY: ${OPENFN_WEBHOOK_KEY}
    volumes:
      - ./fhir-pipes:/config:ro
      - warehouse:/warehouse
    init: true                   # reaps the processes a refresh's tools leave
    restart: unless-stopped
    stop_grace_period: 150s      # more than [serve] stop_grace (120 s)
    logging:
      driver: json-file
      options: { max-size: "50m", max-file: "5" }
```

`serve` reads HAPI as `refresh` does: through the Info Gateway with the
Keycloak client (`server.token_url`), or from HAPI's database with
`fetch = "hapi-db"`. The `serve` API is not FHIR and does not go through the
Info Gateway; callers reach it on the Compose network or through the site's
reverse proxy, which also terminates TLS.

`init: true` runs a small init process as the container's PID 1, with
`serve` under it. A process a refresh's tool leaves behind, once `serve`
has killed it, is a zombie that only PID 1 can reap; without an init, they
pile up in a long-running container.

`restart: unless-stopped` brings `serve` back if it is killed, for example
by the out-of-memory killer: `serve` and its refreshes share the
container's memory, and a refresh running in a child process protects
`serve` from a crash in the refresh, not from the container running out of
memory. `stop_grace_period` gives `serve` time to stop a run cleanly (see
[Stopping](#stopping)).

## The config

```toml
[serve]
retries = 2               # attempts after the first, for a failure that may pass
retry_delay = 600         # seconds between attempts
stop_grace = 120          # seconds a stopping refresh gets before SIGKILL
keep_runs = 100           # finished runs whose records and logs are kept
max_log_bytes = 20_000_000

[[serve.schedule]]
cron = "0 */6 * * *"      # five fields, UTC
mode = "incremental"

[[serve.schedule]]
cron = "0 3 * * 0"
mode = "full"

[[serve.notify]]
name = "openfn"                   # shown in records instead of the URL
url = "https://openfn.example.org/i/2b1c…"
header = "x-api-key"              # optional, with value_env
value_env = "OPENFN_WEBHOOK_KEY"  # the variable holding the header's value
```

The values shown are the defaults, apart from the schedule and notify
entries. A notify `url` must be `http` or `https` and may not carry a user
name or password; put a key in `header` and `value_env`. A mistake in
`[serve]` stops `serve` at start, with exit status 2.

## The API

Every path but `/health` needs the token (the `/fhir` paths of the
[query API](#queries-sql-run) also take the query token). Responses are JSON, apart from
`/metrics` and the log.

| Request | What it does |
|---|---|
| `POST /run` | Start a run. The body is optional (at most 16 KB): `{"mode": "incremental" \| "full", "only": [steps], "skip": [steps], "push": true}`. An empty body is an incremental run of every step. Answers `202` with `{"id", "status_url"}` once the run's record is written; `409` with `{"current": id}` if a run is under way; `400` for an unknown field, step or mode; `503` when the record cannot be written or `serve` is stopping. |
| `GET /status` | `{"current": run or null, "last": the last finished run or null, "next_scheduled": time or null}` |
| `GET /runs?limit=50` | Recent runs, newest first, without each attempt's result (up to 500) |
| `GET /runs/{id}` | A run's record |
| `GET /runs/{id}/log?tail=200` | A run's output as text; without `tail`, all of it (up to 10,000 lines with it) |
| `POST /runs/{id}/cancel` | Stop a run: SIGTERM to its refresh, which stops its tools and holds the stop through its publish, then SIGKILL to everything the run started after `stop_grace`; a run waiting between attempts ends at once. `409` once the run has ended. |
| `GET /metrics` | Prometheus metrics |
| `GET /health` | `ok` |

The steps are `refresh`'s: `locations`, `views`, `tiles`, `libraries`,
`manifest`, `catalog`, `database`, `hooks`, `push`.

A run's record:

```json
{
  "id": "20261021T020000Z-7f3a",
  "trigger": "api",
  "request": {"mode": "incremental", "only": [], "skip": [], "push": false},
  "status": "failed",
  "accepted_at": "…", "started_at": "…", "finished_at": "…",
  "attempts": [
    {"started_at": "…", "finished_at": "…", "exit_code": 1,
     "result": {"status": "failed", "failed_step": "views", "error": "…",
                "committed": false, "published": false, "…": "…"}}
  ],
  "retry": "not retried: 2 retries used",
  "notify": {"openfn": {"state": "delivered", "tries": 1, "last_error": null}}
}
```

`status` is `accepted`, `running`, `waiting` (between attempts),
`succeeded`, `failed`, `cancelled` or `interrupted` (`serve` stopped during
the run). Each attempt's `result` is the refresh's own record, as in
`last-run.json` (see [refresh.md](refresh.md#how-the-last-run-went)).

## Retries

A failed attempt is tried again after `retry_delay`, up to `retries` times,
when it exited 1 and failed at one of `lock`, `tools`, `start`, `files`,
`locations`, `views`, `tiles`, `manifest` or `catalog`. Those parts change
nothing outside the warehouse's staging and downloads, so running them
again repeats nothing.

Nothing else is retried: a config error (exit 2); a failure in the
database step, the publish, a hook or the push, since by then the database
may hold the new rows, the warehouse may be published, or a hook may have
written to another system; a refresh killed by a signal from outside
`serve`, which is most likely the out-of-memory killer and would most
likely kill it again; a cancelled run. The record's `retry` says why an
attempt was or was not retried. Hooks get `RUN_ID`, the same for every
attempt of a run.

## Notify

When a run ends, `serve` posts this to each `[[serve.notify]]` destination:

```json
{
  "event_id": "20261021T020000Z-7f3a:finished",
  "run_id": "20261021T020000Z-7f3a",
  "trigger": "api",
  "status": "failed",
  "attempts": 3,
  "result": {"status": "failed", "failed_step": "views", "error": "…", "…": "…"},
  "run_url": "/runs/20261021T020000Z-7f3a"
}
```

`result` is the last attempt's. A destination is tried 3 times, 10 s and
then 60 s apart, each try with a 10 s connect timeout and 30 s in all.
Redirects are not followed. Each destination's state is kept in the run's
record, so a `serve` restarted before a run was sent sends it then, to the
destinations still pending.

Delivery is at least once: a post whose answer was lost is sent again with
the same `event_id`. A receiver should ignore an `event_id` it has already
seen. In OpenFn, a job can keep the ids it has handled in a collection
(`collections.get` and `collections.set`, keyed by `event_id`) and stop when
the id is already there.

### Alerts

Under `serve`, refreshes run with their failure hooks off
(`FHIR_PIPES_FAILURE_HOOKS=off`): with retries, a failure hook would run
once per attempt. The run's notify, sent once per run with
`"status": "failed"`, `"cancelled"` or `"interrupted"`, is the alert. A site
moving from cron to `serve` moves its failure-hook alerts to a notify
destination: an OpenFn webhook that sends the message, or a small relay for
a chat tool, since the body is fhir-pipes' own JSON. Success hooks run as
before.

## The schedule

Each `[[serve.schedule]]` starts a run, with its `mode`, at the times its
five-field cron expression gives, in UTC. A time that comes while a run is
under way is skipped and counted in the metrics. Times missed while `serve`
was stopped are not run later. `GET /status` gives the next time.

## Logs

Each attempt's output goes to its run's `log` and to `serve`'s stdout, each
line after the run id, so `docker logs` shows everything. A line longer than
64 KB is cut, with a note of how much was dropped. A run's log stops growing
at `max_log_bytes`. `serve`'s stdout goes through a buffer of 10,000 lines
and 8 MB: when whatever reads it falls behind, lines are dropped and
counted rather than holding up the refresh.

### Secrets

`refresh` hides the same values in an error before it cuts it short (an
HTTP error body at 500 characters, `last-run.json`'s `error` at 2,000), so
a cut cannot leave part of one: `serve` names the ones from its config to
each refresh in `FHIR_PIPES_SECRET_VARS`. `serve` replaces these values with `***`
in logs, records and notify bodies: `FHIR_PIPES_TOKEN`, `FHIR_PIPES_CLIENT_SECRET`,
`FHIR_PIPES_PASSWORD`, `FHIR_PIPES_HAPI_DATABASE_URL`, the variable the
`[database]` section names, `FHIR_PIPES_SERVE_TOKEN`, `FHIR_PIPES_QUERY_TOKEN`, each notify
`value_env`, and each notify `url`. A value shorter than 4 characters is
not replaced, since it would replace ordinary text, and `serve` warns at
start. Anything else a site's own hooks print is the site's to keep out of
its logs.

## Queries ($sql-run)

`serve` can run the warehouse's SQLQuery Libraries for a caller and return
the rows, as the SQL on FHIR `$sql-run` operation does. A dashboard, a
notebook or an indicator export names a query and gets its result, without
holding the warehouse's files. The API is off unless the config has a
`[serve.query]` section.

It is a subset of `$sql-run`. Only SQLQuery Libraries run, only ones the
config approves, and only the catalog's own: a caller cannot send SQL.

### The query config

```toml
[serve.query]
approve = "active"        # "active", "listed" or "all"
# queries = ["https://example.org/Library/CoverageByArea"]   # with "listed"
source = "local"          # "local", or a URL prefix such as "r2://bucket/warehouse"
# duckdb_setup = "query-setup.sql"   # SQL run before each query, relative to the config
timeout = 60              # seconds a query may run
max_rows = 100_000        # most rows one query returns
concurrency = 4           # queries that may run at once
```

The values shown are the defaults. Only `queries` and `duckdb_setup` have
none. A mistake in the section stops `serve` at start, with exit status 2:
an unknown key, a `timeout`, `max_rows` or `concurrency` of 0, a `source`
that is neither `local` nor a URL, or `queries` without `approve = "listed"`
(and the reverse).

`serve` builds the query settings from the `fhir-pipes.toml` it was given and
the same environment variables `refresh` reads, so a config `refresh`
accepts also starts the query API. The Library sources, `strip_prefix` and
the `duckdb` command are `refresh`'s.

### Which queries run

| `approve` | Runs |
|---|---|
| `active` | Every SQLQuery whose `status` is `active`. A `draft` does not run. |
| `listed` | The SQLQuery Libraries in `queries`, each given as its url or `url\|version`. |
| `all` | Every SQLQuery, drafts too. |

Only a SQLQuery runs. A SQLView or a ViewDefinition never does, whatever
`approve` says. With `listed`, each entry must be a SQLQuery in the catalog,
or `serve` does not start.

The catalog is what `refresh`'s `libraries` step reads: the `libraries`
sources in the config. `serve` loads it at start, and a failure stops
`serve` with exit status 2. It loads the catalog again after each run that
succeeded, so a changed Library is served once the refresh that fetched it
ends. When that load fails, `serve` keeps the old catalog and logs
`query catalog not reloaded: …`. A catalog error shows no credentials from a
Library source URL.

A SQLQuery whose url contains `|` stops the catalog from loading, since
`url|version` is how a caller names a version. Change the url.

### Tokens

| Variable | Opens |
|---|---|
| `FHIR_PIPES_QUERY_TOKEN` | `/fhir/*` only |
| `FHIR_PIPES_SERVE_TOKEN` | everything |

Callers send either as `Authorization: Bearer <token>`. A caller of the query
API holds the query token and cannot start, read or cancel a run.

With `[serve.query]`, a `--listen` address that is not loopback needs
`FHIR_PIPES_QUERY_TOKEN`, besides `FHIR_PIPES_SERVE_TOKEN`; `serve` refuses to
start without it.

A path with no token set for it is open, which `serve` allows only on a
loopback address. So on loopback, setting only `FHIR_PIPES_QUERY_TOKEN`
leaves the run API (`/run`, `/status`, `/runs`, `/metrics`) open to anything
on the machine. Set `FHIR_PIPES_SERVE_TOKEN` too.

The query token is not passed to the refreshes `serve` starts, and `serve`
hides it in logs and records like its other secrets. Without `[serve.query]`,
every `/fhir` path answers `404`, after the token check.

### The routes

| Request | What it does |
|---|---|
| `GET /fhir/$sql-run` | Run a query. Parameters in the query string. |
| `POST /fhir/$sql-run` | The same, with the parameters in a Parameters resource. |
| `GET /fhir/metadata` | A CapabilityStatement that lists the operation. |
| `GET /fhir/OperationDefinition/fhir-pipes-sql-run` | The OperationDefinition of this subset. |
| `GET /fhir/Library` | A searchset Bundle of the approved SQLQuery Libraries, each with its SQL. It takes no search parameters and answers `400` if given any. |

Any other path under `/fhir` is `404`, and another method on these paths is
`405`.

The parameters are:

| Parameter | Type in a Parameters body | |
|---|---|---|
| `subjectCanonical` | `valueCanonical` | Required. The Library's url, or `url\|version`. A url alone names a Library only when the catalog has one version of it. |
| `_format` | `valueCode` | `ndjson`, `json` or `csv`. |
| `_limit` | `valueInteger` | The most rows to return, at least 1. |

Any other parameter is refused with `400`, as is one given twice.

With GET:

```sh
curl -H "Authorization: Bearer $FHIR_PIPES_QUERY_TOKEN" -G \
  'http://localhost:8090/fhir/$sql-run' \
  --data-urlencode 'subjectCanonical=https://example.org/Library/CoverageByArea' \
  --data-urlencode '_format=csv' \
  --data-urlencode '_limit=1000'
```

With POST:

```sh
curl -H "Authorization: Bearer $FHIR_PIPES_QUERY_TOKEN" \
  -H 'Content-Type: application/fhir+json' \
  'http://localhost:8090/fhir/$sql-run' -d '{
    "resourceType": "Parameters",
    "parameter": [
      {"name": "subjectCanonical",
       "valueCanonical": "https://example.org/Library/CoverageByArea"},
      {"name": "_format", "valueCode": "json"},
      {"name": "_limit", "valueInteger": 1000}
    ]
  }'
```

A POST takes its parameters in the body only, not in the query string. The
body is at most 64 KB, and its `Content-Type` is `application/fhir+json` or
`application/json`.

### The result

The format is `_format` when given. Otherwise it is the best match to
`Accept` among `application/x-ndjson`, `application/json` and `text/csv`,
by q-value (`q` in any case; `q=0` refuses a type). When nothing in
`Accept` is one of these, or there is no `Accept`, the format is NDJSON.
`_format` wins over `Accept`.

| Format | `Content-Type` | Body |
|---|---|---|
| `ndjson` | `application/x-ndjson` | One JSON object per row. |
| `json` | `application/json` | An array of row objects; `[]` for no rows. |
| `csv` | `text/csv` | A header line, then the rows. |

A result is cut at `max_rows`, or at the caller's `_limit` if that is less.
When `max_rows` cut it, the response has `X-Fhir-Pipes-Truncated:
max_rows=<n>`. A result cut by `_limit` has no such header, since the caller
asked for it.

### Statuses

Every error body is an OperationOutcome, apart from `401` and the `404` of
a disabled API, which have no body.

| Status | When |
|---|---|
| `200` | The rows. |
| `400` | A missing, repeated or unknown parameter; a bad `_format` or `_limit`; a body that is not a Parameters resource; a query string on a POST; search parameters on `/fhir/Library`. |
| `401` | No token, or a wrong one. |
| `403` | The Library is in the catalog but is not an approved SQLQuery. The text is the same for a SQLView or a ViewDefinition and for a SQLQuery the config does not approve. |
| `404` | The canonical is not in the catalog; a path that is not one of the routes; any `/fhir` path when `[serve.query]` is not set. |
| `405` | A method the path does not take. |
| `413` | A POST body over 64 KB. |
| `415` | A POST that is not JSON. |
| `422` | The query failed or ran past `timeout`, or the `duckdb_setup` SQL failed. |
| `500` | An error in `serve` itself. |
| `503` | An input table has no data yet, a refresh is publishing, or `concurrency` queries are already running. |

A `503` has `Retry-After: 5`. Its text is fixed: `an input table is not
available yet; retry later`, `a refresh is publishing; retry later` or `too
many queries running; retry later`.

A caller never sees DuckDB's error text, which can quote the setup SQL, and
a `422` carries fixed text: `the query failed; the serve log has the
details`, `the duckdb_setup SQL failed` or `the query timed out after <n>
s`. The details are in `serve`'s log.

Each request makes one line in `serve`'s log:

```
query https://example.org/Library/CoverageByArea csv 200 412 rows 85 ms
query https://example.org/Library/CoverageByArea csv 422 0 rows 31 ms: <detail>
```

A line for an error ends in `: <detail>`, which for a failed query is
DuckDB's message. A result cut at `max_rows` ends in `truncated at <n>`.

### While a refresh publishes

With `source = "local"`, a query reads the Parquet files of the served
warehouse. A refresh replaces them when it publishes. A query that a publish
overlapped is run again once; if the second try is overlapped too, the
answer is `503`.

### A remote source

`source = "r2://…"` or `"s3://…"` reads the Parquet files from object
storage instead, with `duckdb_setup` to give DuckDB the credentials and
settings it needs (an `INSTALL httpfs;` and a `CREATE SECRET`, say). The
setup SQL is read once at start and runs before each query.

A query that runs while `rclone` pushes a refresh to the bucket can read a
mix of old and new files. `serve` cannot tell, as it does with a local
warehouse. This is a limit of the remote source.

### Library parameters

A Library's `[[parameter]]` entries are not supported yet. A query takes
no parameters of its own; `$sql-run` runs it as written. They are planned,
since dashboards and indicator exports will want one district or month.

## Metrics

`GET /metrics` gives:

- `fhir_pipes_runs_total{trigger, status}`
- `fhir_pipes_run_running`
- `fhir_pipes_last_run_timestamp_seconds{status}`
- `fhir_pipes_last_success_timestamp_seconds`
- `fhir_pipes_last_run_duration_seconds`
- `fhir_pipes_retries_total`
- `fhir_pipes_schedule_skipped_total`
- `fhir_pipes_notify_failures_total{destination}`
- `fhir_pipes_record_write_failing`: 1 while a run's end cannot be written;
  no run is started meanwhile
- `fhir_pipes_log_lines_dropped_total`
- `fhir_pipes_table_rows{table}`, from the published `manifest.json`
- `fhir_pipes_queries_total{status}`: `$sql-run` requests by HTTP status
- `fhir_pipes_query_seconds`: a histogram of their duration

An alert on `time() - fhir_pipes_last_success_timestamp_seconds` catches a
warehouse that has stopped refreshing, whatever the reason.

## Stopping

On SIGTERM or Ctrl-C, `serve` accepts no new run (a request gets `503`)
and sends SIGTERM to the running refresh, which stops as [refresh.md](refresh.md#stopping-a-run)
describes: before the database step it leaves the previous warehouse;
from the database step to the end of the publish it finishes the publish
first. After `stop_grace` seconds `serve` sends SIGKILL to the refresh's
process group. The run is recorded as it ended, or `interrupted`, and its
notify is sent in the time left; anything not sent goes at the next start.

A `serve` that is killed (SIGKILL, out of memory) marks the runs it left
unfinished as `interrupted` when it next starts, and sends their notify. If
it cannot write that, it stops with an error naming the run rather than
leave the run `running` and never notified.
It does not stop processes an earlier `serve` left behind, since a process
id in an old record says nothing about who has it now. In Docker a
container restart has already stopped them. Outside a container, a refresh
that outlived its `serve` still holds the warehouse's lock and finishes as
any refresh does, but a tool whose refresh also died is not found.
