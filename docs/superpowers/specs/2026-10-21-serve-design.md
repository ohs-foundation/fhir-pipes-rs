# fhir-pipes serve: design

**Date:** 2026-10-21
**Status:** Design, not yet built. Revised after a builder check and two
design reviews by Codex on 2026-10-21. The first review's findings are
referred to as R1–R11, the second's as F1–F10.
**Builds on:** PR #136 (`last-run.json`, hooks with `on = "failure"`,
`--lock-timeout`).

Three changes to `refresh` come with it (see "Changes to refresh"): a
clean stop on SIGTERM or Ctrl-C, a result file for a supervising process,
and a way to turn failure hooks off when that process does the alerting.

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
  stack, on Linux, with the warehouse on a mounted volume.
- It reads HAPI through the Info Gateway with a Keycloak client
  (client-credentials, already supported), or from HAPI's database with
  `fetch = "hapi-db"`. That is the site's choice and is unchanged by
  `serve`.
- The `serve` API is not FHIR and does not go through the Info Gateway.
  OpenFn calls it on the internal network or through the site's reverse
  proxy.

## Goals

1. Start a refresh over HTTP and get a run id back at once.
2. Report how the run ended to configured destinations (an OpenFn webhook
   trigger) when it ends, success or failure, with the step and error on
   failure.
3. Show the current run, recent runs and each run's output over HTTP.
4. Run refreshes on a built-in schedule, so a site can drop cron and see
   scheduled and triggered runs in one history.
5. Retry a run that failed for a reason that may pass, where a retry cannot
   repeat anything already done outside the warehouse.
6. Alert once per run that has really failed, not once per attempt: the
   run's notify, which says `status: "failed"`, is the alert.
7. Prometheus metrics, and a health check for Docker.

## Non-goals

- A web page or UI.
- More than one warehouse per `serve` process. A site with two runs two.
- A queue. One run at a time; a request while one runs is refused.
- Commands other than `refresh` (`export`, `sync`, `dhis2`). They can come
  later through the same runner.
- Checking Keycloak tokens. A shared token for now (see Access).
- Destinations passed in a request. They come from the config only.
- Running the config's failure hooks. Under `serve` the notify is the
  alert (see Alerts).
- TLS. The site's reverse proxy terminates TLS.
- Warehouse snapshots and their retention (the Java controller's purge).
- Windows. `serve` is built for Unix (Linux in Docker; macOS for
  development) and is not in the Windows binary. *(My call, R10: stopping
  a refresh's tools reliably on Windows needs Job Objects, and no site runs
  Windows.)* `refresh` on Windows keeps today's behaviour on Ctrl-C.

## Anti-goals

These would make it a failure even if it worked:

- A run started by `serve` leaves something other than what the same
  `refresh` from cron would. (The changes to `refresh` below apply to
  both.)
- A crash of a refresh takes `serve` down with it. (Memory is a separate
  case: see "Memory".)
- A retry repeats something a run already did outside the warehouse (a
  database load, a hook's write to DHIS2, a push).
- A secret `serve` knows of, 4 characters or longer (see Secrets), appears
  in an API response, a run record, a log `serve` writes or serves
  (Docker's included), or a notify body.
- A caller can make `serve` send requests to an address the config does
  not list.

## Changes to refresh

### A clean stop on SIGTERM or Ctrl-C (Unix)

Today a signal kills `refresh` at once: `last-run.json` stays `running`, no
failure hooks run, and the tools it started (duckdb, tippecanoe, rclone, a
hook) keep running and may write into `.staging/` while the next run cleans
it up.

**Tools stay in refresh's process group (F1).** `refresh` does not move its
tools into groups of their own. Under `serve`, each attempt is the leader
of a new process group, so a signal to that group reaches `refresh`, every
tool it runs (views and tiles can run tools side by side) and every
process a shell hook starts; `serve`'s SIGKILL after its grace period
reaches all of them even if `refresh` itself is stuck or dead. On a stop,
`refresh` also sends SIGTERM to each tool it started, which matters from
cron, where nobody signals the whole group. There, a process a shell hook
starts in the background may outlive the stop; `docs/refresh.md` says so.
`refresh` lets go of the warehouse's lock only after the tools it started
have been reaped.

**Where a stop takes effect (R1, F2).** From the start of the database step
(or of the publish, without one) to the end of the publish, the run is in
its commit window: the database may already hold the new rows, so the
files must follow.

- Before the commit window, a stop takes effect at the next point the run
  can stop (between steps, between resource types in the views step,
  between pages of a download): the tools are stopped, staging is removed,
  and the previous warehouse and database stay as they were.
- Inside the commit window, a stop is held back until the publish ends
  (successfully or not), and then takes effect.
- After the publish (hooks, push), a stop stops the hook or push. The new
  warehouse stays published.

In each case the run records `failed` in `last-run.json` with
`error: "stopped by signal"` and the step it was in, runs the failure
hooks (unless they are off, below) and exits 1. A second signal skips the
hooks and the wait: tools are killed and the run exits at once.

`last-run.json` gets `committed` and `published` on every run, so a reader
can tell what a failed run left behind. `committed` is `true` once the
database step's commit has succeeded (a later failure there, such as its
`ANALYZE`, keeps it `true`), `"unknown"` when the commit's answer was
lost, `false` otherwise. `published` is `true` once any table has been
swapped in.

A forced stop inside the commit window (SIGKILL, or a second signal) that
comes after the database's commit and before the publish marker leaves the
database with the new run's rows and the files with the old run's. The
next run that succeeds brings them back together. This is the same as a
kill today and is documented, not hidden.

### A result file for a supervisor (R3)

With `FHIR_PIPES_RESULT_FILE=<path>`, `refresh` also writes its final
record (the same fields as `last-run.json`) to that path once it knows how
it ended, with the crash-safe write in `src/fs.rs` (fsync, rename, fsync
of the directory). The path is the supervisor's; `refresh` writes nothing
else there. It is written whether or not the run got the lock, so a run
that fails at `lock` or `tools` reports too.

### Failure hooks off under a supervisor (R4)

With `FHIR_PIPES_FAILURE_HOOKS=off`, `refresh` runs no hooks after a
failure, and says so in its output. Success hooks, `"always"` hooks in the
`hooks` step of a run that succeeds, and `--only`/`--skip` are unchanged.
Without the variable, `refresh` is unchanged from PR #136, so cron runs
keep their failure hooks.

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
retries = 2               # attempts after the first, for a retryable failure
retry_delay = 600         # seconds between attempts
stop_grace = 120          # seconds a stopping child gets before SIGKILL
keep_runs = 100           # finished run records and logs kept
max_log_bytes = 20_000_000

[[serve.schedule]]
cron = "0 */6 * * *"      # five fields, UTC
mode = "incremental"

[[serve.schedule]]
cron = "0 3 * * 0"
mode = "full"

[[serve.notify]]
name = "openfn"                  # shown in records instead of the URL
url = "https://openfn.example.org/i/2b1c…"
header = "x-api-key"             # optional
value_env = "OPENFN_WEBHOOK_KEY" # the header's value, read from this variable
```

A config error in `[serve]` stops `serve` at start with exit 2, as other
config errors do. A notify `url` must be `http` or `https` and must not
have a user name or password. *(R7, R8.)*

### One serve per warehouse (R5)

`serve` holds an exclusive lock on `<data>/.serve/serve.lock` for its
whole life, taken before it reads or repairs any run record. A second
`serve` on the same warehouse fails at start with a message naming the
lock. This is separate from `.refresh.lock`, which the children take.

### Each attempt is a child `fhir-pipes refresh`

`serve` runs each attempt as a child process: the same binary
(`std::env::current_exe()`), `refresh --config <path>`, plus `--full`,
`--only`, `--skip`, `--push` from the request or schedule, and
`--lock-timeout 0`, as the leader of a new process group. Its environment
is `serve`'s plus
`FHIR_PIPES_RESULT_FILE=<data>/.serve/runs/<run id>/attempt-<n>.json`,
`FHIR_PIPES_FAILURE_HOOKS=off` and `FHIR_PIPES_RUN_ID=<run id>`, which
success hooks get as `RUN_ID` so an integration can tell attempts of one
run apart from separate runs (R2).

A refresh from outside `serve` (cron, by hand) that holds the warehouse
makes the attempt fail at once at `lock`.

### The attempt's result (R3)

The child's exit status is authoritative: exit 0 is success, whatever the
result file says. The result file supplies the details. If it is missing,
unreadable, or not a final record, `serve` records the attempt with the
exit status it saw, `failed_step: "unknown"` (for a non-zero exit), and the
last lines of the child's stderr as `error`. A child killed by a signal is
`failed` with `error: "killed by signal N"` and no `exit_code`.

### Runs, attempts and the run record

A run is one request or one scheduled time. It has one or more attempts.
States: `accepted`, `running`, `waiting` (between attempts), `succeeded`,
`failed`, `cancelled`, `interrupted` (serve stopped during the run).

Each run has a directory `<data>/.serve/runs/<id>/`, created exclusively (a
new id is drawn if it exists): `run.json`, the attempts' result files, and
`log` (every attempt's output).

**Control records are durable (F7).** `run.json` is written with the
crash-safe write in `src/fs.rs` at every change of state. A write that
fails is not skipped:

- `POST /run` answers `202` only after the `accepted` record is written;
  if it cannot be written, the answer is `503` and nothing starts.
- A run's end is written, with every destination `pending`, before any
  notify is sent. If that write fails, `serve` retries it every 30 s,
  sends nothing until it succeeds, and starts no other run meanwhile; the
  state shows on `/status` and in the metrics.
- Logs are not control records: a log write that fails is counted and
  printed to stderr, and the run goes on.

```json
{
  "id": "20261021T020000Z-7f3a",
  "trigger": "api",
  "request": {"mode": "incremental", "only": [], "skip": [], "push": false},
  "status": "failed",
  "accepted_at": "…", "started_at": "…", "finished_at": "…",
  "attempts": [
    {"started_at": "…", "finished_at": "…", "exit_code": 1,
     "result": { …that attempt's result file… }}
  ],
  "retry": "not retried: failed at hooks",
  "notify": {"openfn": {"state": "delivered", "tries": 1, "last_error": null}}
}
```

The id is a UTC time and four random hex digits; ids sort by start time
and are checked against that pattern before any path is built from them.

**At start (R5, F6)**, after taking `serve.lock`, `serve` marks each run
left `accepted`, `running` or `waiting` as `interrupted`, with its
destinations `pending`, and sends its notify. It does not restart them and
does not signal any process: a process group number in an old record says
nothing reliable about who owns that number now. In Docker a container
restart has already stopped the old children. Elsewhere, a child that
outlived the old `serve` still holds `.refresh.lock`, so a new attempt
fails at `lock` (and is retried) until it ends; it finishes as a normal
refresh would.

Retention keeps the last `keep_runs` finished runs. A run that is not
finished, or has a destination still `pending`, is never removed (R6,
R11).

### Retries (R2, F3)

One rule. A failed attempt is retried when it exited 1, the run has not
been cancelled, attempts so far are fewer than `1 + retries`, and its
`failed_step` is one of `lock`, `tools`, `start`, `files`, `locations`,
`views`, `tiles`, `manifest`, `catalog`. Those parts change nothing
outside the warehouse's staging and raw downloads, so running them again
repeats nothing.

Every other case is not retried: exit 2; a failure at `database`,
`publish`, `hooks`, `push`, `finish` or `unknown`; a missing result file;
a child killed by a signal from outside `serve` (most likely the
out-of-memory killer, which would most likely kill it again). This holds
whatever the request's `only` and `skip` were, so a hooks-only run that
fails is not retried. The record's `retry` says why a failed attempt was
or was not retried.

Defaults: `retries = 2`, `retry_delay = 600`. *(My call: the user asked
for failures and repeats to be handled; these defaults are a guess.)*

### Alerts

Under `serve`, children run with `FHIR_PIPES_FAILURE_HOOKS=off` and
`serve` runs no hooks of its own: the run's notify is the alert, sent once
per run whatever ended it (a bad view on the first attempt, the last of
three attempts, a failure that is not retried, a cancel, an interruption).
A receiver branches on `status`. Success hooks run in the children as
usual. `docs/serve.md` says that a site moving from cron to `serve` should
move its failure-hook alerts to a `[[serve.notify]]` destination (for a
chat tool, through OpenFn or a small relay, since the body is fhir-pipes'
JSON). *(Agreed with the user. It removes the questions of which run's
`last-run.json` a hook reads, of honouring `--only`/`--skip`, and of
recovering hooks cut off by a restart: F4, F5, F9.)*

### Notify (R6, R7)

When a run ends, `serve` writes the end with every destination `pending`
(see "Control records are durable"), then POSTs a JSON body to each
`[[serve.notify]]` destination:

```json
{
  "event_id": "20261021T020000Z-7f3a:finished",
  "run_id": "20261021T020000Z-7f3a",
  "trigger": "api",
  "status": "failed",
  "attempts": 3,
  "result": { …the last attempt's result, committed and published included… },
  "run_url": "/runs/20261021T020000Z-7f3a"
}
```

- Delivery is at least once. A POST whose response is lost, or a restart
  between a POST and recording it, sends the same `event_id` again; the
  receiver should ignore an `event_id` it has seen. `docs/serve.md` shows
  how in an OpenFn job.
- Each destination has its own state in the record (`pending`,
  `delivered`, `failed` after its tries), so a restart resends only to
  those still `pending`.
- A destination is tried 3 times, waiting 10 s, then 60 s. Each try has a
  10 s connect timeout and a 30 s overall timeout, and reads at most 64 KB
  of the response. Redirects are not followed: a 3xx is a failed try.
  Destinations are sent to one at a time, apart from the runs, so a slow
  destination never holds up a run, a cancel or the API.
- A destination removed from the config is dropped from a pending run at
  the next start; one added is not sent old runs.
- A notify that fails does not change the run's status.

### HTTP API

All responses are JSON except `/metrics` and the log.

| Method and path | What it does |
|---|---|
| `POST /run` | Body (all optional, at most 16 KB): `{"mode": "incremental" \| "full", "only": [steps], "skip": [steps], "push": bool}`. `202` with `{"id", "status_url"}`. `409` with the current run's id if one is accepted, running or waiting. `400` for an unknown field, step or mode. `503` when the run cannot be recorded. |
| `GET /status` | `{"current": run or null, "last": the last finished run or null, "next_scheduled": time or null}` |
| `GET /runs` | Recent runs, newest first, without attempts' results; `?limit=` up to 500 |
| `GET /runs/{id}` | The run record |
| `GET /runs/{id}/log` | The run's output as text; `?tail=N` lines, up to 10,000 |
| `POST /runs/{id}/cancel` | Stop the run: SIGTERM to the child's process group (which stops as under "A clean stop"), SIGKILL after `stop_grace`; or end a `waiting` run. `200`, or `409` when the run has ended. |
| `GET /metrics` | Prometheus text (below) |
| `GET /health` | `200 ok` while the server is up. No token needed. |

The token is checked before a body is read or a run id is looked up.
Steps are the names `refresh` takes (`locations`, `views`, `tiles`,
`manifest`, `catalog`, `database`, `hooks`, `push`).

The API and the schedule start runs through one place that decides, under
one mutex, whether a run is current, so two cannot start at once. A
scheduled time that comes while a run is current is skipped, logged, and
counted in the metrics. A scheduled time missed while `serve` was stopped
is not run later.

### Logs (R8, R11, F8, F10)

Each attempt's stdout and stderr are read line by line. A line longer than
64 KB is cut: `serve` reads up to 64 KB plus the length of the longest
secret, redacts that, keeps the first 64 KB, and drops the rest of the line
with a note of how much it dropped. A secret that starts in the kept part
is therefore always whole in what was redacted, and never shows in part.
Each line, once redacted, goes to the run's `log` file and to `serve`'s
stdout prefixed with the run id, so `docker logs` shows it.

The child's pipes are always read as fast as the child writes. Neither a
slow HTTP client reading a log nor a slow consumer of `serve`'s stdout can
stall a refresh: lines for stdout go through a queue of 10,000 lines, and
when it is full lines are dropped and counted, with a line saying how many
once there is room. A run's `log` stops growing at `max_log_bytes`, with a
line saying how much was dropped. `serve`'s own events (run accepted,
attempt ended, retry, notify delivered or failed, schedule skipped) go to
stdout and the run's log the same way. `docs/serve.md` recommends Docker's
log rotation (`max-size`).

### Secrets (R8)

`serve` replaces, by value, every secret it knows of with `***` in each log
line before it is written anywhere, in every string in run records before
they are serialised, and in notify bodies: the values of
`FHIR_PIPES_TOKEN`, `FHIR_PIPES_CLIENT_SECRET`, `FHIR_PIPES_PASSWORD`,
`FHIR_PIPES_HAPI_DATABASE_URL`, the `[database]` section's `url_env`,
`FHIR_PIPES_SERVE_TOKEN`, each notify `value_env`, and each notify `url`.
Records name destinations, not URLs. A secret shorter than 4 characters is
not replaced (it would replace ordinary text everywhere), and `serve`
prints a warning at start naming the variable. A secret with a line break
in it is replaced line by line, each of its lines being a value to
replace.

What a site's own hook commands print, beyond these values, is the site's
responsibility; `docs/serve.md` says so.

### Access

A bearer token from `FHIR_PIPES_SERVE_TOKEN`, compared in constant time,
for every path except `/health`. An empty variable counts as unset.
Without a token, `serve` refuses to start on anything but a loopback
address. *(My call; Keycloak tokens can come later if a site wants access
managed with the Info Gateway's.)*

### Memory (R9)

A child process protects `serve` from a crash in a refresh (a panic, an
abort), not from the container running out of memory: `serve` and its
children share the container's memory limit, and the kernel's
out-of-memory killer may pick either. `docs/serve.md` says to give the
container `restart: unless-stopped`, so a `serve` killed this way comes
back and marks the run `interrupted` (with its notify) at start. *(My
call: a separate memory limit for the child, through cgroups, is left for
later.)*

### Metrics

- `fhir_pipes_runs_total{trigger, status}`
- `fhir_pipes_run_running` (0 or 1)
- `fhir_pipes_last_run_timestamp_seconds{status}`
- `fhir_pipes_last_success_timestamp_seconds`
- `fhir_pipes_last_run_duration_seconds`
- `fhir_pipes_retries_total`
- `fhir_pipes_schedule_skipped_total`
- `fhir_pipes_notify_failures_total{destination}`
- `fhir_pipes_record_write_failing` (0 or 1)
- `fhir_pipes_log_lines_dropped_total`
- `fhir_pipes_table_rows{table}` from the published `manifest.json`

### Shutdown

On SIGTERM or Ctrl-C, `serve` stops taking requests and sends SIGTERM to a
running child's process group. It waits up to `stop_grace` seconds (120 by
default, long enough for most publishes held back by "A clean stop"), then
sends SIGKILL to the group. A child that ends by itself in that time is
recorded as it ended; one that did not is `interrupted`. A `waiting` run
is `interrupted`. The end of each is written with its destinations
`pending`; `serve` tries to send them in the rest of the grace period, and
any not sent go out at the next start. `docs/serve.md` says to set
Docker's `stop_grace_period` above `stop_grace`.

## Left to the builder

- The HTTP library. Constraints: tokio is already in the tree (hyper too,
  through reqwest); the binary must still build for the Unix release
  targets, musl static included; keep added dependencies small. The notify
  client may reuse the existing reqwest blocking client with redirects
  turned off.
- The cron library (five fields, UTC).
- How `refresh` checks for a stop between its steps, and how it tracks the
  commit window.
- The layout of `docs/serve.md`, and a Docker Compose example beside HAPI
  and the Info Gateway.

## Testing

Integration tests start `serve --listen 127.0.0.1:0` (it prints the address
it bound), against the refresh fixtures with `FHIR_PIPES_REUSE_RAW=1` in its
environment so the children read local files. The schedule and the retry
delay take their time from a clock the tests can set (an internal test
hook, not a five-field cron every second).

`serve`:

- `POST /run` → `202`, the record exists as `accepted` before the answer;
  an unwritable runs directory → `503` and no child; a second request →
  `409`; a run started by the API and one by the schedule at the same
  moment: only one starts.
- Retries: a failure at `views` (exit 1) is retried; a bad view (exit 2)
  is not; a failing success hook after a hook that wrote to a test server
  is not retried, and the test server sees one write; a hooks-only run
  that fails is not retried; a failure at `database` is not retried.
- Alerts: no failure hook runs in any attempt; one notify per run, with
  `status: "failed"` after a bad view on attempt 1 and after the last of
  three attempts, `"cancelled"` after a cancel, `"interrupted"` after an
  interruption.
- The result: exit 0 with a missing result file is success; an outside
  refresh writing `last-run.json` does not change the attempt's result.
- Notify: the body above, once, to a test server; a lost response is sent
  again with the same `event_id`; a restart after destination A is
  delivered and before B resends only to B; a 307 to a second test server
  is a failed try and the second server sees nothing; a server that never
  answers times out; a pending notify survives retention; a failed write
  of the run's end sends nothing until the write succeeds.
- Access: a token is required, except on `/health`; the token is checked
  before the body is read; an empty token with a non-loopback `--listen`
  refuses to start; a run id that is not of the id pattern is `404`.
- A second `serve` on the same warehouse fails at start.
- Cancel stops a running child, the tools it runs side by side, and a
  shell hook's delayed child writer; SIGKILL of the child's group after
  the grace period reaches a tool even when `refresh` ignores the stop.
- `serve` killed while its child runs: the next start marks the run
  `interrupted`, sends its notify, and signals nothing; a new attempt
  fails at `lock` while the old child still runs.
- Logs: a newline-free output is cut at 64 KB lines; a secret placed at
  every offset around the cut never appears, whole or in part; a run's log
  stops at `max_log_bytes`; a slow log client and a blocked stdout do not
  slow the refresh, and the dropped lines are counted.
- Secrets listed above do not appear in records, the served log, stdout,
  or notify bodies, including a value that would need escaping in JSON and
  one with a line break; a short secret prints the warning.

`refresh`:

- SIGTERM before the commit window leaves the previous warehouse and
  database and records `committed: false, published: false`; SIGTERM in
  the database step finishes the publish, then records
  `committed: true, published: true`; a failure of `ANALYZE` after the
  commit records `committed: true`; SIGTERM during a hook stops it and
  leaves the new warehouse; tools run side by side are all stopped and
  reaped before the lock is released; a second signal exits at once.
- `FHIR_PIPES_RESULT_FILE` is written for a run that succeeds, fails at a
  step, and fails at `lock`.
- `FHIR_PIPES_FAILURE_HOOKS=off` runs no hooks after a failure and leaves
  success hooks as they are; without it, behaviour is PR #136's.
