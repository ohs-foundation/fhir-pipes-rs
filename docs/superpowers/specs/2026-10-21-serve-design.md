# fhir-pipes serve: design

**Date:** 2026-10-21
**Status:** Design, not yet built.
**Builds on:** PR #136 (`last-run.json`, hooks with `on = "failure"`, `--lock-timeout`).

Two changes to `refresh` come with it (see "Changes to refresh"): a clean
stop on SIGTERM or Ctrl-C, and failure hooks only on the last attempt.

## Why

`fhir-pipes refresh` runs from cron or CI and exits. Nothing outside the
machine can start a run, ask how the last one went, or be told when one
finishes. At the sites we work with, data comes into HAPI through other
systems, OpenFn being the example: an OpenFn workflow loads ODK or DHIS2
data into HAPI, and the warehouse should refresh after that load, not
hours later on a fixed clock. When the refresh ends, OpenFn should be told
so it can carry on (push aggregates to DHIS2) or raise an alert on a
failure.

OpenFn cannot run a program; its jobs make HTTP calls and time out after a
few minutes. A refresh can take hours. So fhir-pipes needs an HTTP API that
starts a run and returns at once, and a callback that reports the result
when the run ends.

The Java pipeline's controller does the same job (`/run`, `/status`, a
schedule), and its absence is listed as a gap in
`docs/notes/2026-10-15-fhir-data-pipes-comparison.md`.

## The deployment it is built for

- HAPI FHIR behind the FHIR Info Gateway, which proxies FHIR requests and
  checks Keycloak tokens. This is the setup the sites run today.
- `fhir-pipes serve` runs as one more container in the same Docker Compose
  stack, with the warehouse on a mounted volume.
- It reads HAPI through the Info Gateway with a Keycloak client
  (client-credentials, already supported), or from HAPI's database with
  `fetch = "hapi-db"`. That is the site's choice and is unchanged by
  `serve`.
- The `serve` API is not FHIR and does not go through the Info Gateway.
  OpenFn calls it on the internal network or through the site's reverse
  proxy.

## Goals

1. Start a refresh over HTTP and get a run id back at once.
2. Report how the run ended to configured URLs (an OpenFn webhook trigger)
   when it ends, success or failure, with the step and error on failure.
3. Show the current run, recent runs and each run's output over HTTP.
4. Run refreshes on a built-in schedule, so a site can drop cron and see
   scheduled and triggered runs in one history.
5. Retry a run that failed for a reason that may pass (exit 1), and never
   one that failed on a config error (exit 2).
6. Prometheus metrics, and a health check for Docker.

## Non-goals

- A web page or UI.
- More than one warehouse per `serve` process. A site with two runs two.
- A queue. One run at a time; a request while one runs is refused.
- Commands other than `refresh` (`export`, `sync`, `dhis2`). They can come
  later through the same runner.
- Checking Keycloak tokens. A shared token for now (see Access).
- Callback URLs passed in a request. They come from the config only.
- TLS. The site's reverse proxy terminates TLS.
- Warehouse snapshots and their retention (the Java controller's purge).

## Anti-goals

These would make it a failure even if it worked:

- A run started by `serve` leaves something other than what the same
  `refresh` from cron would. (The two changes to `refresh` below apply to
  both.)
- A refresh that crashes, runs out of memory or is killed takes `serve`
  down with it.
- A secret `serve` knows of (see Secrets) appears in an API response, a run
  record, a log served over HTTP or a callback body.
- A caller can make `serve` send requests to an address the config does
  not list.

## Changes to refresh

### A clean stop on SIGTERM or Ctrl-C

Today a signal kills `refresh` at once: `last-run.json` stays `running`, no
failure hooks run, and the tools it started (duckdb, tippecanoe, rclone, a
hook) keep running and may write into `.staging/` while the next run cleans
it up. *(My call, after the builder check found this.)*

`refresh` gets a handler for SIGTERM and SIGINT (Ctrl-C on Windows). On the
first signal it:

1. stops the tool it is running, if any (SIGTERM, then kill after 10 s),
2. stops at the next point it can (between steps, between resource types in
   the views step, between pages of a download),
3. records `failed` in `last-run.json` with `error: "stopped by signal"` and
   the step it was in,
4. runs the failure hooks, and exits 1.

A second signal exits at once. Nothing is published after the signal: a
stop before the swap leaves the previous warehouse, as any failure does.
This helps `docker stop` and cron too, not only `serve`.

### Failure hooks only on the last attempt

With retries, every failed attempt is a full `refresh`, and each would run
its failure hooks: three alerts for one failed run. *(Agreed with the user:
alerts only when the run has really failed.)*

`serve` sets `FHIR_PIPES_ATTEMPT=<n>/<of>` (e.g. `1/3`) in the child's
environment. `refresh` runs `on = "failure"` and `"always"` hooks after a
failure only when `n == of`, and prints that it skipped them otherwise.
Every hook gets `ATTEMPT` (`1/3`, or `1/1` without the variable). Without
`serve` a run is always its own last attempt, so cron runs are unchanged.

## Decisions

### Command and config

```sh
fhir-pipes serve --config fhir-pipes.toml --listen 0.0.0.0:8090
```

`--listen` (env `FHIR_PIPES_SERVE_LISTEN`), default `127.0.0.1:8090`. The
same config file as `refresh`, with a new `[serve]` section; `refresh`
ignores it.

```toml
[serve]
retries = 2               # attempts after the first, for a run that exits 1
retry_delay = 600         # seconds between attempts
keep_runs = 100           # run records and logs kept

[[serve.schedule]]
cron = "0 */6 * * *"      # UTC
mode = "incremental"

[[serve.schedule]]
cron = "0 3 * * 0"
mode = "full"

[[serve.notify]]
url = "https://openfn.example.org/i/2b1c…"
header = "x-api-key"            # optional
value_env = "OPENFN_WEBHOOK_KEY" # the header's value, read from this variable
```

A config error in `[serve]` stops `serve` at start with exit 2, as other
config errors do.

### Each run is a child `fhir-pipes refresh`

`serve` runs each attempt as a child process: the same binary
(`std::env::current_exe()`), `refresh --config <path>`, plus `--full`,
`--only`, `--skip`, `--push` from the request or schedule, and
`--lock-timeout 0`. The child inherits `serve`'s environment, which carries
the secrets as it would for cron. A crash or out-of-memory in the child
cannot take `serve` down, and everything in PR #136 (staging, the lock,
`last-run.json`, failure hooks) applies unchanged.

`--lock-timeout 0`: if a refresh from outside `serve` (cron, by hand) holds
the warehouse, the attempt fails at once with `failed_step: "lock"` instead
of hanging, and is retried like any exit 1.

### The run's result

When the child exits, `serve` reads `<data>/last-run.json`. It uses it only
if its `pid` is the child's; otherwise (the child failed before taking the
lock, or another refresh wrote the file) it builds the result itself:
`status: "failed"`, `exit_code` from the child, `failed_step: "lock"` when
the child's output says so, else `"unknown"`, and `error` from the last
lines of the child's stderr. A child killed by a signal is `failed` with
`error: "killed by signal N"` and no `exit_code`.

### Runs, attempts and the run record

A run is one request or one scheduled time. It has one or more attempts.
States: `running`, `waiting` (between attempts), `succeeded`, `failed`,
`cancelled`, `interrupted` (serve stopped during the run).

The run record is JSON:

```json
{
  "id": "20261021T020000Z-7f3a",
  "trigger": "api",              // or "schedule"
  "request": {"mode": "incremental", "only": [], "skip": [], "push": false},
  "status": "failed",
  "started_at": "…", "finished_at": "…",
  "attempts": [
    {"started_at": "…", "finished_at": "…", "exit_code": 1,
     "result": { …last-run.json of that attempt… }}
  ],
  "notify": [{"url": "https://openfn.example.org/i/…", "status": "sent", "tries": 1}]
}
```

The id sorts by start time. Records and each attempt's combined stdout and
stderr are kept in `<data>/.serve/runs/<id>.json` and `<id>.log`, the last
`keep_runs` runs. `notify` urls are shown without query strings, since a
webhook URL can carry a key.

At start, `serve` marks any record still `running` or `waiting` as
`interrupted`; it does not restart them.

### Retries

An attempt that exits 1 is tried again after `retry_delay`, up to `retries`
more times. Exit 2 is never retried. A child killed by a signal it did not
get from `serve` (most likely the out-of-memory killer) is not retried,
since it would most likely be killed again. *(My call.)* A cancelled run is
not retried. While waiting the run is `waiting` and
still counts as the current run. The defaults are `retries = 2`,
`retry_delay = 600`. *(My call: the user asked for failures and repeats to
be handled; these defaults are a guess at a sensible amount.)*

### Notify

When a run ends (after its last attempt, or when cancelled), `serve` POSTs
a JSON body to each `[[serve.notify]]` URL:

```json
{
  "run_id": "20261021T020000Z-7f3a",
  "trigger": "api",
  "status": "failed",
  "attempts": 3,
  "result": { …the last attempt's last-run.json… },
  "run_url": "/runs/20261021T020000Z-7f3a"
}
```

`result` carries `failed_step`, `error` and `exit_code` on a failure. One
body for every run, to every URL; the receiver branches on `status`. A URL
that fails (connection error, non-2xx) is tried 3 times, waiting 10 s, then
60 s. The outcome is kept in the run record; a notify that never succeeds
does not change the run's status. Notify is sent once per run, not per
attempt.

A run that ends `interrupted` (`serve` stopped during it) is notified when
`serve` next starts and marks it so, with `status: "interrupted"`. The run
record keeps `notify_pending: true` from the moment the run ends until each
URL has been tried, so a notify cut off by a shutdown is sent at the next
start. *(My call.)*

### HTTP API

All responses are JSON except `/metrics` and the log.

| Method and path | What it does |
|---|---|
| `POST /run` | Body (all optional): `{"mode": "incremental" \| "full", "only": [steps], "skip": [steps], "push": bool}`. `202` with `{"id", "status_url"}`. `409` with the current run's id if one is running or waiting. `400` for an unknown step or mode. |
| `GET /status` | `{"current": run or null, "last": the last finished run or null, "next_scheduled": time or null}` |
| `GET /runs` | Recent runs, newest first, without attempts' results; `?limit=` |
| `GET /runs/{id}` | The run record |
| `GET /runs/{id}/log` | The run's output as text; `?tail=N` lines |
| `POST /runs/{id}/cancel` | Stop the run: SIGTERM to the child's process group, kill after 30 s, or end a `waiting` run. `200`, or `409` when the run has ended. |
| `GET /metrics` | Prometheus text (below) |
| `GET /health` | `200 ok` while the server is up. No token needed. |

Steps are the names `refresh` takes (`locations`, `views`, `tiles`,
`manifest`, `catalog`, `database`, `hooks`, `push`).

A scheduled time that comes while a run is current is skipped, logged, and
counted in the metrics. A scheduled time missed while `serve` was stopped is
not run later.

### Secrets

`serve` replaces, by value, every secret it knows of with `***` in the logs
it serves and stores, in run records, and in notify bodies: the values of
`FHIR_PIPES_TOKEN`, `FHIR_PIPES_CLIENT_SECRET`, `FHIR_PIPES_PASSWORD`,
`FHIR_PIPES_HAPI_DATABASE_URL`, the `[database]` section's `url_env`,
`FHIR_PIPES_SERVE_TOKEN`, and each notify `value_env`, when set and at
least 8 characters long. What a site's own hook commands print is the
site's responsibility; `docs/serve.md` says so. *(My call.)*

### Access

A bearer token from `FHIR_PIPES_SERVE_TOKEN`, compared in constant time,
for every path except `/health`. Without the variable, `serve` refuses to
start on anything but a loopback address. *(My call; Keycloak tokens can
come later if a site wants access managed with the Info Gateway's.)*

### Metrics

- `fhir_pipes_runs_total{trigger, status}`
- `fhir_pipes_run_running` (0 or 1)
- `fhir_pipes_last_run_timestamp_seconds{status}`
- `fhir_pipes_last_success_timestamp_seconds`
- `fhir_pipes_last_run_duration_seconds`
- `fhir_pipes_schedule_skipped_total`
- `fhir_pipes_notify_failures_total`
- `fhir_pipes_table_rows{table}` from the published `manifest.json`

### Logs

`serve` prints one line per event (run started, attempt ended, notify sent
or failed, schedule skipped) to stdout, and the child's output prefixed with
the run id, so `docker logs` shows everything.

### Shutdown

On SIGTERM or Ctrl-C, `serve` stops taking requests, sends SIGTERM to a
running child's process group, waits up to 30 s, then kills the group,
marks the run `interrupted` with `notify_pending`, and exits. On Unix each
child starts in its own process group, so the tools it runs are stopped
with it. A stopped refresh is safe: it records the stop itself (above), and
its staging is cleaned up by the next run. Docker's default 10 s stop
timeout is too short for this; `docs/serve.md` says to set
`stop_grace_period: 40s`.

## Left to the builder

- The HTTP library. Constraints: tokio is already in the tree (hyper too,
  through reqwest); the binary must still build for the five release
  targets, musl static included; keep added dependencies small.
- The cron library and its expression syntax (five fields, UTC).
- The exact run id format, as long as ids sort by start time.
- How the child's output is captured and tailed.
- Windows: no SIGTERM; `Child::kill` is enough there.
- The layout of `docs/serve.md`, and a Docker Compose example beside HAPI.

## Testing

Integration tests start `serve --listen 127.0.0.1:0` (it prints the address
it bound), against the refresh fixtures with `FHIR_PIPES_REUSE_RAW=1` in its
environment so the children read local files. Cover at least:

- `POST /run` → `202`; a second → `409`; the run ends `succeeded` and its
  record and log are served.
- A run that fails with exit 1 is retried `retries` times with a short
  `retry_delay`; one that fails with exit 2 is not.
- Notify reaches a test HTTP server with the body above, once per run;
  a failing URL is tried 3 times and the run keeps its status.
- A token is required, except on `/health`; no token and a non-loopback
  `--listen` refuses to start.
- Cancel stops a running child and the tools it started; a restart marks an
  unfinished run `interrupted` and sends its notify.
- `refresh` stopped by SIGTERM records `failed` with "stopped by signal",
  runs failure hooks, and leaves the previous warehouse.
- With `FHIR_PIPES_ATTEMPT=1/3`, failure hooks are skipped; with `3/3` they
  run, and get `ATTEMPT`.
- A refresh holding the lock from outside makes the attempt fail at `lock`.
- A schedule fires (a cron every second, or an injectable clock) and is
  skipped while a run is current.
- No secret listed under Secrets appears in run records, logs served, or
  notify bodies.
