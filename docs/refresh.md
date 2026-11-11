# fhir-pipes refresh

`fhir-pipes refresh` refreshes a Parquet warehouse from a FHIR server, or
from NDJSON and Bundle files (see [From files](#from-files)). It downloads
the data, runs [SQL-on-FHIR](https://sql-on-fhir.org) ViewDefinitions over it,
and writes the result as Hive-partitioned Parquet tables with a manifest. It
can also build map tiles of the Location registry, load PostgreSQL, run your
own commands afterwards, and mirror the warehouse to object storage.

Tiles use duckdb and tippecanoe, pushing uses rclone, and `catalog.duckdb`
uses duckdb. Only the tools the chosen steps need must be installed.
`--help` lists every flag.

## Run

```bash
fhir-pipes refresh --config fhir-pipes.toml
fhir-pipes refresh --only views --reuse-raw          # views from the last download
fhir-pipes refresh --server https://example.org/fhir --server-name prod --push
fhir-pipes refresh --input exports/ --input extra.ndjson   # from files, no server
fhir-pipes refresh --check                           # settings, steps, tools found
```

Exit status: 2 when the input is wrong (invalid config, an invalid
ViewDefinition, a missing `--input`), 1 when something fails while it runs
(server or database unreachable, a failing tool or hook, another refresh still
holding the warehouse after `--lock-timeout`).

### Steps

| Step | What it does | Needs |
|---|---|---|
| `locations` | the Location registry, extracted incrementally into `raw/<server>/locations` and transformed into `parquet/locations` | |
| `views` | download each resource type the views read (only what changed since the last refresh; by search, by `$export` with `fetch = "bulk"`, or from the HAPI server's database with `fetch = "hapi-db"`), run the views, write the tables | |
| `tiles` | the registry to `tiles/admin.pmtiles`, `facilities`, `settlements`, `catchments` | duckdb (spatial), tippecanoe |
| `libraries` | run each SQLQuery Library over this run's tables and write its result as a table; only with `libraries` in the config. See [Libraries](#libraries) | duckdb |
| `manifest` | `manifest.json`: time, server, each table's row count, the views | |
| `catalog` | `catalog.sql`: a DuckDB view per table; only with a `[catalog]` section. See [Catalog](#catalog) | duckdb, only for `duckdb_file` |
| `database` | the view tables, `location_hierarchy` and the Library results into PostgreSQL, and the SQLViews as views; only with a `[database]` section. See [Database](#database) | |
| `hooks` | the config's commands, in order. See [Hooks](#hooks) | the commands |
| `push` | `rclone sync` of the configured directories and files; only with `--push` | rclone |

`locations` runs first, then `views` and `tiles` side by side, then
`libraries`, the manifest, the catalog and the database. The tables and files are then
published one by one (a direct reader may briefly see data from different runs), and the hooks and the push run last.

Every step except `push` runs by default. `--only views,tiles` picks steps
(the steps that depend on them are added), `--skip tiles` leaves some out, and
`--push` adds the push. The binary checks that each tool the chosen steps need
is installed before anything runs.

### Incremental and full

After the first run, `locations` and `views` download only what changed since
the last refresh. `--full` downloads everything again. `--reuse-raw` uses the files in
`raw/<server>/` where they exist; missing or empty view inputs are fetched. The views step keeps
`server.concurrency` requests in flight (default 4); see the
[README](export.md#parallel-downloads). With `fetch = "bulk"` in `[server]`
it uses a Bulk Data `$export` instead of search (`bulk_timeout` in seconds,
default 14400); see the [README](export.md#bulk-data-export). The
`locations` step always uses search.

### Reading the HAPI server's database

With `fetch = "hapi-db"`, the views step reads the types the views use from
the PostgreSQL database of a HAPI FHIR JPA server instead of its API (see the
[README](export.md#reading-a-hapi-servers-database)). The connection string
comes only from `FHIR_PIPES_HAPI_DATABASE_URL`.

### What a failed run leaves behind

A run writes its tables, `views/`, `tiles/`, `manifest.json` and `catalog.sql`
to staging directories (`parquet/.staging/` and `.staging/`) and swaps them
into place only after every step up to and including `database` has
succeeded. A run that fails before that leaves every table and file as the
previous successful run left them, and removes its staging.

What a failed run does change:

- `raw/`: the downloads move forward, so the next run downloads only what
  changed since.
- PostgreSQL: the `database` step commits before the swap, so if the swap
  fails afterwards, the database has the new run's rows.

A run killed before the swap leaves its staging behind; the next run removes
it, and finishes a swap that was killed part way. A refresh holds a lock on
`.refresh.lock` in the warehouse directory, so a second refresh of the same
warehouse waits for the first; `--lock-timeout SECONDS` limits the wait (exit
1; `0` fails at once). Each publish adds one to the count in
`.publish-generation`, which `serve`'s query API reads to tell that a publish
overlapped a query.

### Stopping a run

On Unix a refresh stops cleanly on SIGTERM or Ctrl-C, as `docker stop` sends.
Before the database step it removes its staging and the warehouse stays as it
was; during the database step and the publish it waits for the publish to end.
It records `failed` with `stopped by signal` in `last-run.json`, runs its
failure hooks and exits 1. A second signal exits at once.

### How the last run went

`last-run.json` in the warehouse directory says how the last run went, or that
one is running. A run writes it only while it holds the lock, first with
`"status": "running"`, then `"succeeded"` or `"failed"`. A run that is killed
leaves `running`, with its process id in `pid`. A run that fails before it gets
the lock does not write it.

```json
{
  "status": "failed",
  "started_at": "2026-10-21T02:00:00+00:00",
  "committed": false,
  "published": false,
  "failed_step": "views",
  "error": "views: exporting Observation: ... 503 Service Unavailable",
  "exit_code": 1
}
```

`committed` says whether the database step's commit happened (`true`,
`false`, or `"unknown"` when the answer was lost). `published` is `true` once
anything live in the warehouse has changed. Together they say what a failed
run left behind. `server` is redacted: without a user name, password or query
string. A run that failed has `failed_step`, `error` and `exit_code`.

Each run that finishes also writes `raw/<server>/last-refresh.json`, with its
step timings and `type_mismatches` (see [Tables](#tables)).

#### Environment variables for a supervisor

- `FHIR_PIPES_RESULT_FILE`: path for this attempt's own result. Set a unique one per attempt, including lock failures, because `last-run.json` may belong to another attempt.
- `FHIR_PIPES_FAILURE_HOOKS=off`: suppresses the failure hooks, so a supervisor that alerts itself does not send duplicates.
- `FHIR_PIPES_RUN_ID`: the `RUN_ID` passed to hooks.
- `FHIR_PIPES_SECRET_VARS`: comma-separated names of variables whose values (four bytes or longer) are hidden in HTTP error bodies and refresh error summaries. A tool's own output is not filtered.

### Tables

Each ViewDefinition writes `parquet/<table>/<column>=<value>/part-0.parquet`.
The table is the view's name in snake case minus a configured prefix
(`IcrIndicatorValue` to `indicator_value`); it must not be `locations` or
`location_hierarchy`. Dates and dateTimes use native types: partial dates are floored, date columns drop the time, and a value that does not fit its column becomes null with a warning.

Every row gets a partition column (default `country`), looked up through
another table by the first `[[partition.lookup]]` rule whose `key` column the
view has. A row without a match goes to `__HIVE_DEFAULT_PARTITION__` (with a warning). A view with a column named like the
partition column stops the run: set `[partition] column` to another name.

A view that is removed or renamed leaves its table behind in `parquet/`;
delete the directory by hand.

## From files

With a file source, `refresh` reads the resources from files instead of a FHIR
server, which is never contacted.

```toml
[source]
files = ["../synthea/fhir", "exports/*.ndjson"]   # files, directories, glob patterns
```

or `refresh --input PATH`, repeated, which replaces the config's list.

The inputs are NDJSON, a Bundle, a JSON resource or an array of them (`.gz` is
read as gzip). A path that does not exist, or a pattern that matches nothing,
stops the run. The resources are split by type into `raw/<name>/<Type>.ndjson`
(`name` is `--server-name` or `server.name`, `files` by default). The steps
then run as for a server. File inputs replace the raw dataset: supply the complete
set each run, because a missing resource type gives empty view tables. Without
Location resources, existing Location tables stay; `tiles` is skipped only when
there is no `locations` table. The manifest's `server` is `null`.

## Catalog

With a `[catalog]` section, each refresh writes `catalog.sql`: one DuckDB
`CREATE OR REPLACE VIEW` per table, reading the table's Parquet with
`hive_partitioning` and `union_by_name`. A dashboard or an analyst then queries
`coverage`, not `parquet/coverage/country=*/**/*.parquet`. Writing the file
needs no DuckDB.

```toml
[catalog]
base_url = "https://data.example.org/warehouse"   # where the views read the tables
extra = "catalog-extra.sql"                        # your own views, appended
duckdb_file = true                                 # also build catalog.duckdb (needs duckdb)
```

`base_url` decides how the views name the files: unset, relative to the
warehouse directory; `https://…`, each table's files listed one by one (HTTP
cannot list a directory); `s3://…`, `r2://…`, `gcs://…` or a path, a glob under
it.

`catalog.duckdb` holds the same views in a DuckDB file that a client attaches
read-only (`ATTACH 'https://…/catalog.duckdb' AS warehouse (READ_ONLY);`).

### Querying with Spark

With `spark = true`, each refresh also writes `catalog.spark.sql`: Spark SQL
that drops and creates an external table per warehouse table. Keys:
`spark_location`, `spark_database` (default `fhir`), `spark_extra`. Use a
database of its own: the file drops any table in it with a warehouse table's
name.

## Database

With a `[database]` section, each refresh also loads the view tables into
PostgreSQL, for dashboards and tools that read a database. A build
without the `postgres` feature stops a refresh that has `[database]`.

```toml
[database]
url_env = "FHIR_PIPES_DATABASE_URL"   # the variable holding the connection string (the default)
schema = "public"                      # the default
tables = ["indicator_value", "coverage"]   # default: every view table
mode = "replace"                       # the default; "upsert" rewrites only what changed
```

The connection string (`postgres://user:password@host:5432/db`) is read only
from the environment, never from the file. Its `sslmode` sets TLS as for
`psql`; managed databases usually need at least `require`.

Each view table becomes a table of the same name, with the same columns and
the partition column. The `Location` table is not loaded. The step also loads
`location_hierarchy` and each SQLQuery Library's result, and makes each SQLView
Library a PostgreSQL view from its `postgresql` SQL. Views and the privileges
granted on them are kept across runs.

All tables are replaced in one transaction, so a reader sees the previous
run's rows until the commit, then all of the new run's. A failure before the
commit rolls back; a failure during or after it (for example `ANALYZE`) may leave
the new rows committed, so check `committed`. A run without the `views` step
(for example `--only libraries`) loads only the derived tables and the SQLViews,
unless `--only` names `database`.

With `mode = "upsert"`, a run rewrites only the rows of the resources that
changed since the last load, using a change log in `raw/<server>/.changes.json`.
Each loaded view needs a column whose path is `getResourceKey()` (named `id`
by preference); a view without one stops the run with exit 2. Upsert loads a
table in full when there is no change log, the run has `--full`, or the table
is new or changed. It records its loads in a table `fhir_pipes_loads` in the
same schema.

The schema is created only when it does not exist, which needs `CREATE` on the
database. Loading into an existing schema needs `USAGE` on it and `SELECT`,
`INSERT` and `DELETE` on the tables. Creating tables needs `CREATE` on the
schema, recreating a changed table needs ownership, and upsert also needs
`UPDATE` on `fhir_pipes_loads`. Manual edits to reused raw files or database
rows are not tracked; use a full or replacement load to reconcile them.

## Libraries

The `libraries` step runs SQL on FHIR SQLQuery Libraries over the warehouse
and writes each result as a table. It runs only when the config has
`libraries`, a list of sources:

```toml
libraries = [
  "../libraries/out",              # a directory of JSON, relative to this file
  "example.analytics#0.1.0",       # a FHIR package in the package cache
  "https://example.org/libs.tgz",  # a FHIR package .tgz
]
```

An `https://` package is downloaded again on every run; a `name#version`
package must already be in `~/.fhir/packages`. A source's SQL runs with the
refresh's file access: use only sources you trust (see
[Sources](libraries.md#sources)).

Each SQLQuery Library runs after the Libraries it depends on and writes
`parquet/<table>/` (`IcrCoverageByArea` becomes `coverage_by_area`). A result
with a `country` column is partitioned by it; any other result is one file.
The results are published with the other tables. `fhir-pipes libraries build`
turns `.sql` files into Library JSON; see [libraries.md](libraries.md).

## Settings

Command-line flags win over their environment variables, which win over the
config file (`--config`, else `./fhir-pipes.toml` or `./fhir-warehouse.toml`
when present). Paths in the file are relative to the file.

| Config key | Flag / environment | Default |
|---|---|---|
| `data` | `--data`, `FHIR_PIPES_DATA` | (required) |
| `views` | `--views`, `FHIR_PIPES_VIEWS` | (required for `views`) |
| `profile` | `--profile`, `FHIR_PIPES_PROFILE` | base FHIR. See [profiles.md](profiles.md) |
| `libraries` | | none: the `libraries` step does not run |
| `server.base` | `--server`, `FHIR_PIPES_SERVER` | `http://localhost:3447/fhir` |
| `server.name` | `--server-name` | `hapi-local`; `files` with a file source |
| `server.kind` | `--server-kind` | `generic`; `medplum` or `aidbox`. See [servers.md](servers.md) |
| `server.fetch`, `server.concurrency` | | `search` (or `bulk`, `hapi-db`); 4 |
| `source.files` | `--input` (repeatable) | none: resources come from the server |
| | `--reuse-raw`, `--full`, `--threads` | download again; only what changed; number of CPUs |

Authentication is a token (`FHIR_PIPES_TOKEN`), a username with
`FHIR_PIPES_PASSWORD`, a client ID and secret (`FHIR_PIPES_CLIENT_ID`,
`FHIR_PIPES_CLIENT_SECRET`, with `server.token_url`), or `server.google_auth`;
use only one. Client credentials, passwords and connection strings are read only from the
environment; a fixed token also accepts `--token`. See the
[README](../README.md#connecting-to-a-fhir-server).

```toml
data = "../data"
views = "../ig/fsh-generated/resources"
libraries = ["../libraries/out"]
[server]
base = "https://fhir.example.org/fhir"
name = "prod"
[[partition.lookup]]        # a view with location_id: country from parquet/locations
key = "location_id"
table = "locations"

[push]
remote = "r2"               # rclone remote
bucket = "warehouse"
[[hooks]]
name = "alert"
on = "failure"              # "success" (the default), "failure" or "always"
run = ["sh", "-c", "curl -fsS -X POST --data-binary @\"$DATA/last-run.json\" \"$ALERT_URL\""]
```

Other sections: `[tables] strip_prefix`, `[tools]` (paths of `duckdb`,
`tippecanoe`, `rclone`), `[tiles] countries`, `[catalog]` and `[database]` (below).

### Hooks

A hook runs in the config file's directory with `DATA` (the warehouse
directory), `FHIR_BASE` and `SERVER_NAME` set. `on` says when:

- `on = "success"`, the default: after the tables, manifest and catalog are
  swapped in, with `STATUS=succeeded`. A hook that fails stops the run, which
  then fails at `hooks`.
- `on = "failure"`: after a failed run, with `STATUS=failed`, `FAILED_STEP`,
  `ERROR` and `EXIT_CODE`. If it fails, that is only a warning; the run still
  fails with its own error.
- `on = "always"`: both.

`after` names a step (`locations`, `views`, `tiles`, `libraries`, `manifest`,
`catalog` or `database`); the hook then runs only when that step ran. It
applies to success hooks only.

### Push

`--push` runs `rclone sync` for the `dirs` and `files` in `[push]` to
`<remote>:<bucket>`; `--dry-run` shows what it would upload.
