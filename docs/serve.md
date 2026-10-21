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

Every path but `/health` needs the token. Responses are JSON, apart from
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

The steps are `refresh`'s: `locations`, `views`, `tiles`, `manifest`,
`catalog`, `database`, `hooks`, `push`.

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
a cut cannot leave part of one. `serve` replaces these values with `***`
in logs, records and notify bodies: `FHIR_PIPES_TOKEN`, `FHIR_PIPES_CLIENT_SECRET`,
`FHIR_PIPES_PASSWORD`, `FHIR_PIPES_HAPI_DATABASE_URL`, the variable the
`[database]` section names, `FHIR_PIPES_SERVE_TOKEN`, each notify
`value_env`, and each notify `url`. A value shorter than 4 characters is
not replaced, since it would replace ordinary text, and `serve` warns at
start. Anything else a site's own hooks print is the site's to keep out of
its logs.

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
