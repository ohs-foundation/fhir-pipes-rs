# Approved SQLQueries over HTTP: `$sql-run` in `fhir-pipes serve`

Status: draft for review, 2026-10-29.

## Intent

`fhir-pipes serve` gets an HTTP API that lets other people and systems get
results from the FHIR data and the warehouse in a controlled way, without
access to the data itself. Uses named so far: exporting indicators, and
powering a dashboard. A caller names an approved SQLQuery Library by its
canonical URL and gets its rows back.

The API follows the SQL on FHIR 3.0.0-ballot `$sql-run` operation
(https://build.fhir.org/ig/HL7/sql-on-fhir/en/OperationDefinition-SQLRun.html),
as a subset that the server declares, so clients written for the
specification can call it.

Why rather than querying DuckDB or PostgreSQL directly: a caller needs only
a URL and a token, not read access to the Parquet files or the database,
and can run only queries that have been reviewed. Approval is a merge into
the analytics directory.

## Goals

- Run an approved SQLQuery by `subjectCanonical` and return its rows.
- Read the data from the local warehouse directory or from the published
  copy in R2 (or any S3-compatible store), chosen in the config.
- Control which queries can run, with the rule chosen in the config.
- Keep the query API's access separate from the API that starts refreshes.

## Non-goals for the first version

These are left out on purpose. Each can be added when someone needs it.

- **Library parameters (TODO).** Dashboards and indicator exports will want
  results for one district or one month. This needs `Library.parameter`
  support and safe binding (the specification forbids putting values into
  the SQL text), which in turn needs DuckDB in-process (the `duckdb` crate)
  or PostgreSQL prepared statements instead of the `duckdb` command. Until
  then a caller gets the whole result and filters it.
- Running ViewDefinitions, either by canonical over the warehouse (that
  returns row-level data) or inline with inline `resource` data.
- Inline SQL: `subjectResource` and `context`. These need the DuckDB
  sandbox (no external access, reads limited to the warehouse) first.
- `subjectReference`, `patient`, `group`, `_since`, `source`, `header`.
- `$sql-export` (asynchronous jobs).
- A token per caller with its own list of queries. One shared query token
  for now.
- `_format=parquet` and `_format=fhir`.

## Anti-goals

- A caller must never be able to run SQL that has not been approved, or
  read a table directly.
- A query must never return a mix of two refreshes' data from a local
  warehouse.
- The query token must not be able to start, cancel or read refresh runs.

## The API

Under `/fhir` (my call; the specification's `[base]`):

| Request | What it does |
|---|---|
| `GET` or `POST /fhir/$sql-run` | Run one approved SQLQuery |
| `GET /fhir/metadata` | The CapabilityStatement |
| `GET /fhir/OperationDefinition/fhir-pipes-sql-run` | Our subset OperationDefinition |
| `GET /fhir/Library` | The approved SQLQuery Libraries, as a searchset Bundle, so callers can see what they may run (my call; read-only, no search parameters). Full Library resources, SQL included (my call: the SQL is reviewed, and it tells a caller how a number is computed) |

`$sql-run` parameters supported:

| Parameter | Notes |
|---|---|
| `subjectCanonical` | Required. `url` or `url\|version`. |
| `_format` | `ndjson` (default), `json`, `csv`. Takes precedence over `Accept`, as the specification requires. CSV has a header row. |
| `_limit` | Caps the rows returned. The server may also apply its own maximum (`max_rows`). |

GET takes them in the query string. POST takes a `Parameters` resource with
the same names. Any other parameter, including `subjectResource`,
`subjectReference`, `context`, `resource` and `parameters`, is refused with
`400` and an `OperationOutcome`, as the specification requires of a subset
server.

Responses follow the specification: the raw rows with the format's media
type (`application/x-ndjson`, `application/json`, `text/csv`); errors as
`OperationOutcome` with `400` (bad request), `404` (not in the catalog),
`403` (in the catalog but not approved, or not a SQLQuery, such as a
SQLView or ViewDefinition; my call, so a caller can tell "not approved"
from "does not exist"; `404` for both would hide which queries exist, which
is also defensible), `422` (the SQL fails), `503` with `Retry-After` (the
query could not run now: a publish under way, the concurrency limit
reached, or an input table not there yet), `500` otherwise.

The output is buffered, not streamed (my call): with `max_rows` it is
small, and buffering lets a timeout or a SQL failure still become a proper
error status. `json` is one JSON array of row objects. `csv` is RFC 4180,
as DuckDB writes it, with a header row.

When `max_rows` cuts a result (the caller's `_limit` did not), the response
says so in a header, `X-Fhir-Pipes-Truncated: max_rows=<n>`, and the log
line notes it (my call: no silent cut for an indicator export, without
refusing the request).

The CapabilityStatement lists `$sql-run` with `definition` pointing to our
OperationDefinition, which has `base` set to the specification's
OperationDefinition and declares only the parameters above. It lists the
formats supported.

## Which queries are approved

The catalog is the one the refresh uses: the views directory and the
config's `libraries` sources (`library::resolve::Catalog`). From it, only
SQLQuery Libraries can be run. SQLViews and ViewDefinitions are never
subjects. The rule for which SQLQueries is set in the config:

- `approve = "active"` (default): SQLQueries with `status = active`.
- `approve = "listed"`: only the canonicals in `queries = [...]`,
  whatever their status. Each must be a SQLQuery in the catalog, or
  `serve` refuses to start. An entry without `|version` matches as
  `Catalog::get` does.
- `approve = "all"`: every SQLQuery in the catalog, drafts included. For an
  internal deployment where everyone is trusted. Still no inline SQL.

**Loading the catalog and settings.** `serve` today holds only the
`[serve]` section and runs refreshes as a child process. For queries it
also loads the warehouse `Settings` (views directory, `libraries`,
`strip_prefix`, lookups) from the same config, as `refresh` does, and builds
the `Catalog` the same way (`libraries_step::catalog`). My call: it loads
them at startup and again after each refresh run it finishes, so a merged
and refreshed query becomes available without a restart; a config change
still needs one. A catalog that fails to load at startup stops `serve`; one
that fails to reload keeps the previous catalog and logs the error.

Approval in practice is a reviewed merge of the `.sql` and `.toml` into the
analytics directory with `status = "active"`. The Uganda Libraries are
`draft` today, so they would need to be marked `active` (or listed) to be
served.

## How a query runs

The same way the refresh's `libraries` step and `fhir-pipes libraries
sql` run one: compose the Library with `library::compose::compose` for the
`duckdb` dialect, binding each input to its table and reading it the same
way (the same Parquet glob with `hive_partitioning`, `union_by_name` and
`hive_types_autocast = false`), with SQLViews inlined, and run it with the
`duckdb` command. A SQLQuery that depends on another SQLQuery reads that
one's result table, as the refresh does. The query runs live over the input
tables rather than reading the SQLQuery's materialised result table,
because the materialised result is partitioned by `country` and loses the
Library's `ORDER BY` (the Uganda loaders re-sort it for this reason).

The output is the format the caller asked for, written by DuckDB (e.g.
`COPY (...) TO '/dev/stdout' (FORMAT json)` gives ndjson). `_limit` and
`max_rows` wrap the composed SQL in an outer `LIMIT`, which the
specification allows.

Limits (config, with defaults that are my call):

- `timeout`: 60 s per query; the `duckdb` process is killed after it.
- `max_rows`: 100,000.
- `concurrency`: 4 queries at once; more get `503` with `Retry-After`.

The `duckdb` processes are registered with `stop` so that stopping `serve`
stops them.

## Where the data comes from

Config, `[serve.query] source`:

- `source = "local"` (default): the warehouse directory `serve` looks
  after. Inputs are read from `parquet/<table>/`.
- `source = "s3://bucket/prefix"` (or `r2://`, or any prefix DuckDB reads),
  e.g. `r2://icr/uga`: inputs are read from `<source>/parquet/<table>/`
  with DuckDB's httpfs, the same layout as local with a different prefix.
  Credentials come from a DuckDB setup SQL file
  (`duckdb_setup`, e.g. a `CREATE SECRET`), as `[catalog] duckdb_setup`
  already does, and are redacted from errors as the catalog step does.

**Not reading half-published data, local.** `publish` renames one table at
a time and marks the window with `.staging/.publishing`. Checking the
marker only before a query is not enough: a publish can start during it.
My call: `publish` also bumps a generation number (a small file next to
the marker, written after the last rename). A query reads the marker and
the generation before it runs and again after. If a publish was under way
or the generation changed, the result is thrown away and the query runs
once more; if that also overlaps a publish, the answer is `503` with
`Retry-After`. Queries take no lock, so they never hold up a refresh or a
publish. This changes `stage.rs`, which is in scope.

**Remote.** `rclone sync` is not atomic, so a query during a push can read
a mix of old and new files. The first version documents this limit. A
later fix is publishing to versioned prefixes and switching over by
`versions.json`.

## Access

- A new token, `FHIR_PIPES_QUERY_TOKEN`, sent as `Authorization: Bearer`.
  It is accepted on the `/fhir` paths only.
- `FHIR_PIPES_SERVE_TOKEN` keeps its current meaning and is also accepted
  on `/fhir` (my call: an operator can do everything a query caller can).
- The query API is enabled by the `[serve.query]` section. Without it, the
  `/fhir` paths answer `404` and nothing changes for existing deployments.
- Every `/fhir` path, `metadata` included, needs a token when one is set.
  The token check runs before anything else. With no tokens set, `serve`
  listens only on loopback and the paths are open there, as the rest of
  the API is today.
- The rule that a non-loopback `--listen` needs a token applies to the
  query token too: with `[serve.query]` and a non-loopback address, `serve`
  refuses to start unless `FHIR_PIPES_QUERY_TOKEN` is set.

## Config

```toml
[serve.query]
approve = "active"        # "active" | "listed" | "all"
queries = []              # canonicals, for approve = "listed"
source = "local"          # or "r2://icr/uga", "s3://…"
duckdb_setup = "query-setup.sql"   # for a remote source: CREATE SECRET …
timeout = 60
max_rows = 100000
concurrency = 4
```

`deny_unknown_fields`, as the rest of `[serve]`.

## Logging and metrics

Each query writes one line to `serve`'s stdout: the canonical, the format,
rows returned, time taken and status. `/metrics` gains a counter of queries
by status and a histogram of durations.

## Testing

- Unit: parameter parsing (GET and POST), refusal of every unsupported
  parameter, the three approval rules, format negotiation, `_limit` and
  `max_rows`, the token rules.
- Integration (as `tests/refresh_libraries.rs` does, with the real
  `duckdb`): a warehouse with a view table and a SQLQuery over a SQLView;
  `$sql-run` returns the same rows, in the Library's order, as the
  refresh's result table; a draft query is refused under `approve =
  "active"`; a query during a publish does not see mixed data.
- The SQL on FHIR test suite does not cover SQLQuery over materialised
  tables, so there is nothing to run from it here.

## Left to the builder

- Module layout (e.g. `src/serve/query.rs`) and how the `/fhir` routes
  join the existing router and auth middleware.
- The exact `duckdb` invocation per format.
- The CapabilityStatement's and OperationDefinition's full contents within
  the rules above.
- Docs: a section in `docs/serve.md`.
