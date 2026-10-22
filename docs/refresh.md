# fhir-pipes refresh

`fhir-pipes refresh` refreshes a Parquet warehouse from a FHIR server, or
from NDJSON and Bundle files (see [From files](#from-files)). It
downloads the data, runs [SQL-on-FHIR](https://sql-on-fhir.org)
ViewDefinitions over it, and writes the result as Hive-partitioned Parquet
tables with a manifest. It can also build map tiles of the Location registry,
run your own commands afterwards, and mirror the warehouse to object storage.

This command was fhir-warehouse, which ran kiln as a separate program. The
ViewDefinitions run in process with fhir-pipes-sof (`crates/sof`) on every
CPU, each resource type read once however many views use it. The FHIR side is
the same code as `fhir-pipes run` and `fhir-pipes export`. Tiles use duckdb
and tippecanoe; pushing uses rclone; the optional `catalog.duckdb` uses duckdb.
fhir-pipes never links DuckDB. Only the tools the chosen steps need must be
installed.

## Run

```bash
fhir-pipes refresh --config fhir-pipes.toml
fhir-pipes refresh --only views --reuse-raw          # views from the last download
fhir-pipes refresh --server https://example.org/fhir --server-name prod --push
fhir-pipes refresh --input exports/ --input extra.ndjson   # from files, no server
fhir-pipes refresh --check                           # settings, steps, tools found
```

A run exits 2 when its input is wrong: an unknown or invalid key in the
config file or the profile, a config file that cannot be read, `--only
catalog` without a `[catalog]` section, an `--input` that is not there, or a
ViewDefinition that is not valid. It exits 1 when something fails while it
runs: a server or database that cannot be reached or answers with an error, a
missing or failing tool, a failing hook, a file that cannot be written, or
another refresh that still holds the warehouse after `--lock-timeout`.
Earlier versions exited 1 for every error.

Every run that gets the warehouse's lock records how it went in
`last-run.json` (see [How the last run went](#how-the-last-run-went)), and
hooks can run after a run that failed as well as after one that succeeded
(see [Hooks](#hooks)).

### Steps

| Step | What it does | Needs |
|---|---|---|
| `locations` | the Location registry, as `fhir-pipes run` does it: extracted incrementally into `raw/<server>/locations`, transformed into `parquet/locations`; with `--reuse-raw` and a snapshot there, transformed without an extract | |
| `views` | download each resource type the views read, as `fhir-pipes export --incremental` does (only what changed since the last refresh; by search, by `$export` with `fetch = "bulk"`, or from the HAPI server's database with `fetch = "hapi-db"`), run each type's views on one read of it, 20,000 lines at a time, and write the type's tables as soon as its views are done | |
| `tiles` | the registry → `tiles/admin.pmtiles`, `facilities`, `settlements`, `catchments` | duckdb (spatial), tippecanoe |
| `manifest` | `manifest.json`: time, server (see below), each table's row count, the views, the Location extract's state | |
| `catalog` | `catalog.sql`: a DuckDB view per table, plus your own views; with `spark = true`, also `catalog.spark.sql`: a Spark SQL table per table; only with a `[catalog]` section. See [Catalog](#catalog) | duckdb, only for `duckdb_file` |
| `database` | the view tables into PostgreSQL; only with a `[database]` section. See [Database](#database) | |
| `hooks` | the config's commands, in order | the commands |
| `push` | `rclone sync` of the configured directories and files; only with `--push` | rclone |

`locations` runs first. `views` and `tiles` then run side by side, since both
read only the registry. The manifest, the catalog and the database follow.
The run's tables, manifest and catalog are then swapped in together (see
below), and the hooks and the push run last, on the swapped-in warehouse.

By default every step except `push` runs, `catalog` only when the config has a
`[catalog]` section, and `database` only with `[database]`. `--only
views,tiles` picks steps (the manifest and the catalog are added when a data
step is picked, and the database when `views` is, since it loads the view
tables), `--skip tiles` leaves some out,
and `--push` adds the push. Before anything runs, the binary checks that each
tool the chosen steps need is installed, and stops if one is missing.

### What a failed run leaves behind

A run does not change the warehouse's tables, `views/`, `tiles/`,
`manifest.json` or `catalog.sql` until every step up to and including
`database` has succeeded. Until then it writes them to staging directories:

| Staged at | Swapped in to |
|---|---|
| `parquet/.staging/<table>/`, for each table the run writes, `locations` included | `parquet/<table>/` |
| `parquet/.staging/_report.json` (the Location transform's report) | `parquet/_report.json` |
| `.staging/views/` (the `views` step's copies of the views) | `views/` |
| `.staging/tiles/` (the `tiles` step's archives) | `tiles/` |
| `.staging/manifest.json`, `.staging/catalog.sql` | `manifest.json`, `catalog.sql` |

`views/` and `tiles/` are replaced as a whole when the step that writes them
ran: `views/` then holds the current views only, and `tiles/` only the
archives this run built. A step that did not run leaves its directory as it
is.

While the run goes on, each step reads a table from staging if the run wrote
it, and from `parquet/` if not: the views look partitions up in the new
`locations` and in lookup tables written earlier in the run, the tiles are
built from the new `locations`, and the manifest, the catalog and the database
step describe or load the tables as they will be after the swap. Tables the
run does not write (`locations` with `--only views`, say) stay as they are and
are counted and listed as before.

After the `database` step, each staged table is renamed into `parquet/` (the
old one is renamed aside and then removed), then `views/` and `tiles/` the
same way, then `manifest.json` and `catalog.sql` are renamed into place. A
reader can see files from two runs only during those renames. A table is
never half-written.

So a run that fails in `locations`, `views`, `tiles`, `manifest`, `catalog`
or `database` leaves every table, `views/`, `tiles/`, `manifest.json` and
`catalog.sql` exactly as the previous successful run left them, and removes
its staging. What it does change:

- `raw/`: the downloads and the extract state move forward, so the next run
  downloads only what changed since and rebuilds every table from the whole
  download.
- PostgreSQL: the `database` step commits before the swap, so if a rename in
  the swap fails afterwards, the database has the new run's rows.

`catalog.duckdb` with relative paths is built just after the swap, since
DuckDB reads each table's schema from its files at their final paths. If
that build fails, the run stops with the new tables, `manifest.json` and
`catalog.sql` in place and the previous `catalog.duckdb`.

The hooks run after the swap, on the published warehouse: a hook such as a
catalog build reads the new tables and can write beside them. A hook that
fails stops the run before the push, with the new tables, manifest and
catalog in place. The push uploads the swapped-in warehouse.

A run that is killed before the swap leaves its staging behind; the next run
removes it before it starts. A swap that fails or is killed part way, after
some tables are in place and before the others are, is finished by the next
run before it does anything else: the database already holds that run's rows,
and the warehouse would otherwise have tables from two runs. A run killed
between the two renames of a table's swap leaves the old table as
`parquet/.<table>.old` (or `.views.old`, `.tiles.old` in the warehouse
directory); the next run renames it back. While it runs, a refresh holds a lock on `.refresh.lock` in the
warehouse directory, so a second refresh of the same warehouse waits for the
first to finish. With `--lock-timeout SECONDS` (or
`FHIR_PIPES_LOCK_TIMEOUT`), it waits at most that long and then fails (exit
1) without changing anything; `--lock-timeout 0` fails at once. A scheduler
that retries a failed run can use it so that retries do not queue up behind
a long run.

### Stopping a run

On Unix a refresh stops cleanly on SIGTERM or Ctrl-C (SIGINT), as
`docker stop` and `kill` send. It stops the tools it is running (duckdb,
tippecanoe, rclone, a hook) with SIGTERM, and with SIGKILL those still
running 10 s later, and waits for them before it lets go of the warehouse.
Where the stop takes effect depends on where the run is:

- Before the database step (or the publish, without one), at the next
  point the run can stop: between steps, between resource types in the
  views step, between pages of a download. Staging is removed; the
  warehouse and the database stay as they were.
- From the start of the database step to the end of the publish, it waits
  for the publish to end, so the files follow the rows the database may
  already have.
- After the publish (hooks, push), at once: the hook or push is stopped,
  and the new warehouse stays published.

The run then records `failed` with `stopped by signal` in
`last-run.json`, runs its failure hooks and exits 1. A second signal stops
the tools with SIGKILL and exits at once, without the failure hooks and
without waiting for a publish. A run stopped that way after the
database's commit and before the publish has started leaves the database
with the new rows and the files with the previous run's, until the next
run that succeeds; a run stopped during the publish is finished by the
next run, as above.

A process a shell hook starts in the background is not stopped with the
hook unless the whole process group is signalled (as `fhir-pipes serve`
does).

On Windows, Ctrl-C ends a refresh at once, as before.

### How the last run went

`last-run.json` in the warehouse directory says how the last run went, or
that one is running. A run writes it only while it holds the lock: first
with `"status": "running"`, then `"succeeded"` or `"failed"`. It is written
to a temporary file and renamed into place, so a reader never sees half of
it. A run that is killed leaves `running`, with its process id in `pid`. A
run that fails before it gets the lock (another run still holds it after
`--lock-timeout`, or a config error) does not write it.

```json
{
  "status": "failed",
  "pid": 41207,
  "started_at": "2026-10-21T02:00:00+00:00",
  "server": "https://fhir.example.org/fhir",
  "server_name": "prod",
  "full": false,
  "steps": ["locations", "views", "tiles", "manifest", "catalog", "hooks"],
  "finished_at": "2026-10-21T02:04:12+00:00",
  "seconds": 252.3,
  "timings": {"locations": 31.0},
  "committed": false,
  "published": false,
  "failed_step": "views",
  "error": "views: exporting Observation: GET https://fhir.example.org/fhir/Observation: 503 Service Unavailable",
  "exit_code": 1
}
```

`committed` says whether the database step's commit happened: `true`,
`false`, or `"unknown"` when the commit was sent and its answer was lost.
It stays `true` when something after the commit failed (its `ANALYZE`).
`published` is `true` once anything live in the warehouse has changed: a
table, `views/` or `tiles/` swapped in, or `manifest.json`,
`catalog.sql`, `catalog.spark.sql` or `catalog.duckdb` replaced, and stays
`true` when the run fails afterwards. Together they say what a failed run
left behind.

`server` is redacted as in the manifest: without a user name, password or
query string. `steps` are the steps the run was to take. `timings` are the parts that
finished, in seconds. A run that succeeded has `refreshed_at` (the
manifest's time) and `exit_code` 0. A run that failed has `failed_step`,
`error` (cut to 2,000 characters) and `exit_code`, 1 or 2 as above.
`failed_step` is a step, or the part of the run between them: `tools` (a
tool the steps need is missing), `start` (the staging or the change log),
`files` (splitting a file source by type), `publish` (the swap) or `finish`
(the run's report).

Unlike `raw/<server>/last-refresh.json` (see [last-refresh.json](#last-refreshjson)), it
records failures, so a monitor can tell how the last run ended without
having seen its exit status.

Disk use doubles for the tables, views and tiles a run writes, until the swap.

### Tables

Each ViewDefinition writes `parquet/<table>/<column>=<value>/part-0.parquet`.
The table is the view's name in snake case minus a configured prefix
(`IcrIndicatorValue` → `indicator_value`). It must be a SQL name (a letter, then
letters, digits and `_`), and must not be `locations`, which the Location
transform writes; a view that would give another name stops the run. Dates,
dateTimes, instants and times are parquet date and timestamp types, not text.

The value in the directory name is escaped the way hive escapes it, `%2F` for
`/`, and DuckDB's `hive_partitioning` reads it back unescaped. Escaped are the
characters a path or a Windows file name cannot hold, control characters,
`%`, a trailing dot or space, and the first letter of a Windows device name
such as `CON`. Two values that differ only in case, `Kano` and `KANO`, would
share one directory on macOS or Windows, so the second gets every letter
escaped (`%4B%41%4E%4F`).

A value that does not match its column's type is written as follows:

- an integral number (`5.0`) in an `integer` or `integer64` column (FHIR
  `integer`, `positiveInt`, `unsignedInt`, `integer64`) is written as the
  integer; `5.5` is not rounded and is written as null;
- a dateTime with a time (`2026-06-15T23:30:00-05:00`) in a `date` column is
  written as its date part as written (`2026-06-15`), without converting to
  UTC first. Switch the column to `dateTime` to keep the time;
- a partial date (`2026`, `2026-06`) is floored to the start of the period:
  `2026-01-01`, `2026-06-01`. In a `dateTime` or `instant` column, a partial
  or date-only value is midnight UTC on that day. These are not counted;
- anything else that cannot be converted (a string in an `integer` column, an
  unparseable date) is written as null.

The values written as their date or as null are counted per column and
printed as soon as the views step is done, before the later steps run, so
a hook or push that fails afterwards does not hide them:

```
values that did not match their column's type:
   indicator_value.period_start: 12 dateTime values written as their date
   obs.value: 3 values did not fit integer and were written as null
```

The run does not fail. The counts are also in `last-refresh.json` (see
below).

Every row gets a partition column (default `country`), looked up through
another table: the first `[[partition.lookup]]` rule whose `key` column the view
has gives the table and column to look the value up in. A row without a match
goes to the null partition, `__HIVE_DEFAULT_PARTITION__`, as hive and DuckDB
name it. The views step prints a warning with the number of rows that went
there, for each table with a lookup rule: rows without a key, rows whose key
is not in the lookup table (a key that is not text is never found, since the
lookup table's keys are read as text), or every row when the lookup table is
missing. The run carries on. A view with no rows writes one empty file there, so the table keeps
its columns. A view with a column of the partition column's name stops the
run, since that column would be lost beside the partition column: set
`[partition] column` to another name (the fhir-data-pipes views have an
address `country` column, for one). The `locations` table stays partitioned
by `country` whatever the view tables use. The manifest, the catalog, the
database step and the partition lookups read each table by its own
first-level partition directories, so a lookup through `locations` gives the
Location's country as the row's partition value either way. Tables other views look up in are written first, and are read
back from staging: the resource types whose views write them run first, and
their tables are written before any other. Every other type's tables are
written as soon as its views are done. Tables are swapped in at the end of the run (see [What a
failed run leaves behind](#what-a-failed-run-leaves-behind)).

The views directory can be any directory of JSON files: files whose
`resourceType` is `ViewDefinition`, or ends in `/ViewDefinition` as in an IG
build (`fsh-generated/resources/Binary-*.json`), are views; the rest are
skipped. Each view is checked against the spec's structure rules before
anything runs, and a plain copy is saved to `views/<name>.json`; `views/` is
replaced with the run's copies when the run is published, so a removed view
leaves it and the manifest.

A view that is removed or renamed leaves its table behind in `parquet/`.
refresh does not delete it: it is still counted in the manifest, named in
the catalog and pushed. A run that ran the views step ends with a note
naming each table in `parquet/` that no current view writes, leaving out
`locations` and the lookup tables, and `--check` lists them as
`unwritten:`. Delete a table's directory by hand if it is no longer wanted.

### Parallel downloads

The views step keeps `server.concurrency` search requests in flight at once
(4 by default), as `fhir-pipes export --concurrency` does (see the
[README](../README.md#parallel-downloads)): several resource types download
side by side, and a full download of a large type (10,000 resources or more)
is split into `_lastUpdated` windows paged side by side. Incremental
downloads stay one stream per type. `concurrency = 1` downloads one type at a
time, one page at a time. With `fetch = "bulk"`, it is the number of the
export's output files downloaded at once. The `locations` step pages
Locations as one stream either way.

```toml
[server]
base = "https://fhir.example.org/fhir"
concurrency = 4
```

### Downloading with Bulk Data $export

By default the views step pages FHIR searches. With `fetch = "bulk"` in
`[server]`, it asks the server for a system-level Bulk Data `$export` of the
types the views read instead, as `fhir-pipes export --bulk --incremental`
does (see the [README](../README.md#bulk-data-export)):

```toml
[server]
base = "https://fhir.example.org/fhir"
fetch = "bulk"
bulk_timeout = 14400        # seconds to wait for the export; then it is cancelled
```

The downloads land in the same `raw/<server>/<TYPE>.ndjson` files, so the
views and `--reuse-raw` work the same way. The export's `transactionTime`, or
the server's `Date` on the kick-off when that is earlier, is the next
refresh's `_since`, and the resources the export lists as deleted
are removed. A refresh that stops while the server is still exporting polls
the same export next time. `--full` exports every type without `_since`.
The `locations` step still uses search. `Group/[id]/$export` and
`Patient/$export` are not supported yet.

### Reading the HAPI server's database

With `fetch = "hapi-db"`, the views step reads the types the views use from
the PostgreSQL database of a HAPI FHIR JPA server instead of its API, as
`fhir-pipes export --hapi-db --incremental` does (see the
[README](../README.md#reading-a-hapi-servers-database)). The connection
string comes only from `FHIR_PIPES_HAPI_DATABASE_URL`:

```toml
[server]
base = "https://fhir.example.org/fhir"   # still used by the locations step
fetch = "hapi-db"
```

This ties the refresh to HAPI's internal schema, which changes between HAPI
releases; fhir-pipes checks it before reading and stops on one it does not
know. The files are the same `raw/<server>/<TYPE>.ndjson`. Deleted resources
are removed from them on each refresh, and `--full` reads every type again.
One connection reads the types one after another; `server.concurrency` is
not used. The `locations` step still uses search. `--check` says whether
the environment variable is set.

## From files

With a file source, `refresh` reads the resources from files instead of a
FHIR server, as the Java pipeline's `--fhirFetchMode=NDJSON` and `JSON` do.
The server is never contacted, and the server settings are not needed.

```toml
[source]
files = ["../synthea/fhir", "exports/*.ndjson"]   # files, directories, glob patterns
```

or `refresh --input PATH`, repeated for more inputs, which replaces the
config's list. Paths in the config are relative to the config file; `--input`
paths are relative to the current directory.

The inputs are read as `views run --input` reads them: NDJSON (`.ndjson`), a
Bundle, a JSON resource or an array of them; a directory with its
subdirectories, hidden files skipped; in a Bundle, a reference to another
entry by its `urn:uuid` fullUrl is rewritten to that entry's `Type/id`, so
joins work. A file ending in `.gz` is read as gzip, through every member:
a directory reads `.json`, `.ndjson`, `.json.gz` and `.ndjson.gz` files. A
glob pattern reads the files of those kinds and the directories it matches,
in path order. A path that does not exist, or a pattern that matches
nothing, stops the run, and so do inputs that include both `X.ndjson` and
`X.ndjson.gz` (or `X.json` and `X.json.gz`): they most likely hold the same
resources, so remove one.

The steps then run as for a server:

- The resources are split by type into `raw/<name>/<Type>.ndjson`, each file
  written beside its target and renamed into place. `name` is
  `--server-name` or `server.name`, `files` by default. A resource that
  appears more than once (same type and id) is kept once: the copy with the
  latest `meta.lastUpdated`, or, on a tie or when neither has one, the copy
  read last (the later file in path order). A `<Type>.ndjson` from an earlier
  run, of a type the files no longer have, is removed. The inputs are read
  twice rather than held in memory: first to find each resource's type, id
  and `meta.lastUpdated` and which copy is kept, then to copy the kept ones.
  A resource on an NDJSON line of its own is copied as it is written there;
  one from a Bundle, an array or a JSON file is written out again. A gzip
  NDJSON input is read only once: its lines are written, as they are, to a
  temporary file in `raw/<name>/` that the second pass copies from, so that
  file can grow to the decompressed size of the gzip inputs.
- `locations`: the files' Location resources, and their Organizations if
  there are any, become the snapshot `raw/<name>/locations`, which is
  transformed into `parquet/locations` as after an extract. Boundaries given
  inline (`valueAttachment.data`) are read; boundaries given by URL are not
  fetched, and the transform reports them as unresolved. If the files have
  no Locations, the step is skipped with a note, and an earlier run's
  `parquet/locations`, if there is one, is kept. Without any `locations`
  table, `tiles` is skipped too, and views that look their partition up in
  `locations` write their rows to the null partition.
- `views`: each view runs on its type's file, as on a download. A type the
  views read but the files lack gives each of its views an empty table, with
  a note.
- `manifest`, `catalog`, `database`, `hooks` and `push` are unchanged, and the
  run is staged and swapped in as for a server. The manifest's `server` is
  `null` and its `files` lists the inputs; hooks get an empty `FHIR_BASE`.

With a file source the server's auth settings are not used, so they are not
checked either: a config with `server.username` runs from files without
`FHIR_PIPES_PASSWORD`. The tiles step's tools are checked only once the run
knows the step will run, since files without Locations skip it.

`raw/<name>/` can hold a server's download when the file source uses the
same name (`server.name`). The split removes the server export's state
(`.export-state.json`), and a Location snapshot built from files has no
watermark, so the next refresh from the server downloads every type, and
extracts every Location, in full, rather than merging the server's changes
into the files' copies.

`--reuse-raw` reruns on the last split in `raw/<name>/` without reading the
inputs again. `--check` shows the file source and how many files each input
names, and leaves out the server and auth lines.

On fhir-data-pipes' Synthea sample, with its 12 views (`--skip tiles`, since
the sample's Locations have no admin-unit boundaries, and
`[partition] column = "part"`, since the views have a `country` column), the
tables have the same rows as `views run docker/config/views --input
synthea-hiv/sample_data_segments`.

## Catalog

With a `[catalog]` section, each refresh writes `catalog.sql`: one DuckDB
`CREATE OR REPLACE VIEW` per table, reading the table's Parquet with
`hive_partitioning` and `union_by_name`. A dashboard or an analyst then queries
`coverage`, not `parquet/coverage/country=*/**/*.parquet`, and the files can be
repartitioned or moved without breaking their queries. The file is written from
the tables the refresh wrote, so a new table is in it at once. It is text;
writing it needs no DuckDB.

```toml
[catalog]
base_url = "https://data.example.org/warehouse"   # where the views read the tables
extra = "catalog-extra.sql"                        # your own views, appended
duckdb_file = true                                 # also build catalog.duckdb (needs duckdb)
duckdb_setup = "catalog-setup.sql"                 # run first when building it, e.g. CREATE SECRET
```

`base_url` decides how the views name the files:

| `base_url` | The views read | Use |
|---|---|---|
| unset | `parquet/<table>/country=*/**/*.parquet`, relative to the warehouse directory | `cd data && duckdb -init catalog.sql` |
| `https://…` | each table's files, listed one by one: HTTP cannot list a directory, so a glob does not work there | a public bucket or a Worker in front of it; DuckDB in a browser (DuckDB-Wasm) |
| `s3://…`, `r2://…`, `gcs://…`, a path | a glob under it, as for relative paths | servers and build steps with credentials for the bucket |

Partition columns come from the paths either way, so `WHERE country = 'NGA'`
scans only that country's files. DuckDB still reads each file's footer when it
plans a query (`union_by_name`), one small request per file over HTTP.

`catalog.duckdb` holds the same views in a DuckDB database file of a few
hundred kilobytes, which a client attaches read-only, by URL once it is pushed:

```sql
ATTACH 'https://data.example.org/warehouse/catalog.duckdb' AS warehouse (READ_ONLY);
SELECT * FROM warehouse.coverage WHERE country = 'NGA';
```

DuckDB reads each table's schema from its files when it creates a view, so
the file is built where the views can reach the data. With relative paths
that is just after the run's tables are swapped in. With a `base_url` it is
the push: after the tables
are uploaded, `refresh --push` builds `catalog.duckdb` against the uploaded
copy, then uploads `catalog.sql` and `catalog.duckdb` last, so a client never
sees a catalog naming files that are not there yet. `duckdb_setup` runs before
the views when building it, for credentials such as
`CREATE SECRET (TYPE s3, PROVIDER credential_chain)`. A secret created there
is temporary and is not saved in the file.

A client without `catalog.duckdb` can run `catalog.sql` itself: DuckDB-Wasm
can fetch the file and run its text, and the duckdb shell can run it with
`-init` or `.read`.

### Querying with Spark

With `spark = true`, each refresh also writes `catalog.spark.sql`: Spark SQL
that creates a table for each warehouse table, for a Spark Thrift server such
as the one the fhir-data-pipes Java pipeline runs. Writing it needs no Spark.

```toml
[catalog]
spark = true
spark_location = "s3a://warehouse"           # how Spark reaches the warehouse directory
spark_database = "fhir"                      # the default
spark_extra = "catalog-spark-extra.sql"      # your own Spark SQL, appended
```

`spark_location` is the warehouse directory as Spark sees it: `s3a://…`,
`hdfs://…`, `file:/…`, or a path on the Spark machine (`/dwh` when the
warehouse is mounted there in a container). Unset, it is the warehouse
directory on this machine, as a `file:` URL. Each table reads
`<spark_location>/parquet/<table>`.

The file creates the database if it is missing, then, for each table, drops
the table and creates it again:

```sql
CREATE DATABASE IF NOT EXISTS `fhir`;
USE `fhir`;

DROP TABLE IF EXISTS `fhir`.`coverage`;
CREATE TABLE `fhir`.`coverage` (
  `id` STRING,
  `location_id` STRING,
  `period_start` DATE,
  `country` STRING
)
USING PARQUET
OPTIONS (recursiveFileLookup 'true', pathGlobFilter 'part-*.parquet')
LOCATION 's3a://warehouse/parquet/coverage';
```

Run it after each refresh, with `spark-sql -f catalog.spark.sql` or
`beeline -u jdbc:hive2://<host>:10000 -f catalog.spark.sql`. A hook runs after
the catalog step, so it can do this on every refresh:

```toml
[[hooks]]
name = "spark"
run = ["sh", "-c", "beeline -u jdbc:hive2://spark:10000 -f \"$DATA/catalog.spark.sql\""]
```

`refresh --push` uploads the file with the other catalog files, after the
tables.

- The tables are external (they have a `LOCATION`), so `DROP TABLE` removes
  only the definition, never the files. Use a database of their own: the
  file drops any table in it with a warehouse table's name, and dropping a
  managed table deletes its data.
- Dropping and creating the table each run picks up new columns and new
  files. A query that runs between a table's `DROP` and `CREATE` fails; the
  gap is a few milliseconds per table. The Java pipeline instead creates a
  timestamped table per run and points a view at it; fhir-pipes does not,
  since every run's table would read the same directory and the old tables
  would pile up in the metastore.
- The columns and their types are read from the Parquet files when the file
  is written and listed in the `CREATE TABLE`, so an empty table (one empty
  file) has its columns too. A column Spark cannot read, such as a `time`
  column (Spark has no TIME type), is left out, with a comment saying so.
- The tables are not partitioned in Spark's sense. `country` is a column in
  the files as well as in the directory names, and Spark does not partition a
  table whose partition column is in its files. Spark reads every
  `part-*.parquet` under the table directory instead (other files there, such
  as a catalog's `items.parquet` or `.json` files, are left out). A filter
  such as `WHERE country = 'NGA'` is pushed down to the Parquet statistics,
  so other countries' row groups are skipped, but each file's footer is
  still read.
- The Locations table's `geometry` column is WKB in a Parquet GEOMETRY
  column; the file declares it `BINARY`. Spark 4.1 reads it. Spark 3.5 and
  4.0 (including the `bitnamilegacy/spark:4.0.0` image in the fhir-data-pipes
  compose files) cannot read any column of that table: their Parquet reader
  fails on the GEOMETRY type in the file footer. The view tables work in
  3.5, 4.0 and 4.1.
- Your own views go in `spark_extra`, appended after a `USE` of the
  database, so they can name the tables without the database:
  `CREATE OR REPLACE VIEW coverage_nga AS SELECT * FROM coverage WHERE country = 'NGA';`.
  The `extra` file for DuckDB is not used: the two dialects differ.

Superset connects to the Thrift server with the Hive driver (PyHive), as in
the fhir-data-pipes Superset tutorial, with the database in the URI:
`hive://hive@<host>:10000/fhir`. The tables then appear under the `fhir`
schema.

## Database

With a `[database]` section, each refresh also loads the view tables into
PostgreSQL, for dashboards and tools that read a database (as the
fhir-data-pipes single-machine setup does). The release binaries include it;
a build without the `postgres` feature (`--no-default-features`) stops a
refresh with `[database]` before it runs (`--skip database` runs the rest).

```toml
[database]
url_env = "FHIR_PIPES_DATABASE_URL"   # the variable holding the connection string (the default)
schema = "public"                      # the default
tables = ["indicator_value", "coverage"]   # default: every view table
mode = "replace"                       # the default; "upsert" rewrites only what changed
```

The connection string (`postgres://user:password@host:5432/db`) is read only
from the environment, never from the file. Its `sslmode` sets TLS, as for
`psql`: `require` encrypts the connection; `verify-ca` and `verify-full` also
check the server's certificate, against Mozilla's root certificates (built
in, so nothing on the machine is needed) or the CA file named by
`sslrootcert`. Managed databases (Cloud SQL, RDS, Azure) usually need at least
`require`:

```sh
export FHIR_PIPES_DATABASE_URL='postgres://warehouse:…@db.example.org:5432/analytics?sslmode=verify-full'
```

Each view table becomes a table of the same name, with the same columns and
the partition column (`country`), its types taken from the Parquet files:
`text`, `bigint`, `double precision`, `boolean`, `date`, `timestamp with time
zone` and `time`. The Location table is not loaded.

The step reads the tables the run has staged, before they are swapped into
`parquet/`, so a database failure leaves the files as they were too. All the
tables are replaced in one transaction: each is created if missing,
emptied with `DELETE`, and filled with `COPY`. A reader sees the previous run's
rows until the commit, then all of the new run's, never a mix. Views and grants
built on the tables survive, since a table is dropped and recreated only when
its columns change; if something depends on it then, the step stops and says
so, and the database keeps the previous run. A table with an `id` column gets
an index on it when it is created.

A table whose Parquet cannot be read (a wrong `--data`, a missing mount) stops
the step before anything in the database changes. Two refreshes loading into
the same schema at once take turns: the second waits for the first to commit.

With `mode = "replace"` (the default), every run reloads every table. On
the ICR warehouse (10 view tables, 229,082 rows, 161,216 in the largest)
that takes about half a second.

### Upsert

With `mode = "upsert"`, a run rewrites only the rows of the resources that
changed since the last load, as fhir-data-pipes' `JdbcResourceWriter` does.
For each table, in the same single transaction:

```sql
DELETE FROM t WHERE id = ANY($1);   -- the ids that changed or were deleted
DELETE FROM t WHERE id IS NULL;     -- rows without an id, copied again
COPY t (...) FROM STDIN;            -- the rows of those ids, from the Parquet
```

The rows come from the Parquet files the views step wrote in this run,
filtered by id: the views still run over every resource, so `parquet/` is
the same in either mode, and the database gets the same rows as a full load.
A table whose type has no changes is left as it is. Rows without an id (from
a resource that has none) are not named by any id, so they are deleted and
copied again in every run that finds such rows in the Parquet or the table.

**The id column.** Each loaded view needs a column whose path is
`getResourceKey()`, in a `select` that keeps the resource as its focus: not
under `forEach`, `forEachOrNull` or `repeat` (where it keys the element, such
as a contained resource), and not in a `unionAll`. If several match, the one
named `id` is used, else the first. A view without one stops the run before
anything is downloaded, with exit 2; add
`{"name": "id", "path": "getResourceKey()"}` to its `select`, leave its table
out of `tables`, or use `replace`. The column gets an index if it has none.

**What changed.** The run keeps a change log, `raw/<server>/.changes.json`:
per resource type, the ids that changed or were deleted since the last load.
It is written before each type's file is replaced, so a run that stops
part way never misses an id:

- An incremental search, a bulk `$export` with `_since`, or an incremental
  read of HAPI's database: the ids it read, and the ids in the export's
  `deleted` list or deleted in HAPI's database.
- A type downloaded in full (after a count mismatch, a full bulk export, a
  type with no watermark): the new file is compared with the old one by id
  and by each resource's text. The ids that differ or are new count as
  changed, and the ids gone from the file as deleted.
- A file source (`--input`, `[source] files`): each type's split file is
  compared with the previous split the same way, so only the resources that
  differ are rewritten. A type that is no longer in the files is reloaded.

A type with no old file to compare, or with more than half of its resources
changed (and over 10,000), is marked for a full reload instead of listing
its ids.

**When a table is loaded in full.** Upsert falls back to a full load, as
`replace` does it, for every table when:

- there is no change log yet (the first upsert run, or after a run in
  `replace` mode, which removes it);
- the last refresh did not finish (the log is marked as running from the
  start of a run to its end);
- the run has `--full`;
- the views step did not run in this refresh (`--only database`), since the
  Parquet may then be older than the log; such a load also clears the
  log's record of the tables, so the next run with the views loads in full
  too.

And for one table when:

- the table is new or its columns changed;
- the table was last written by another load. Each upsert load records a
  mark of its own (a UUID) for every table it loads in a table of the same
  schema, `fhir_pipes_loads` (`table_name`, `mark`), in the same
  transaction, and the change log records the mark. A table without this
  warehouse's mark there was written by another warehouse, a `replace` load
  (which removes the marks of the tables it loads or empties), a restored
  backup, or a database the connection string now reaches instead. Upsert
  creates `fhir_pipes_loads` with the comment `fhir-pipes load records,
  version 1`, and uses a relation of that name only when it is an ordinary
  table with that comment and exactly the columns `table_name text` and
  `mark text`. Any other relation of that name (your own table, a view) is
  never written to: upsert stops with exit 2 and names it, and `replace`
  loads as usual and notes that it left it alone;
- its view calls `now()`, `today()` or `timeOfDay()` in any expression
  (columns, `where`, `forEach`, nested selects, `unionAll`), as the views
  engine's FHIRPath parser reads it, so comments and delimited names such
  as `` `now`() `` count too, and so does an expression that does not
  parse: its rows can change while its resources do not, so the table is
  loaded in full in every run;
- its type is marked for a full reload;
- what its rows are built from changed: its view, the partition column, the
  partition lookup and the contents of the lookup table, or the fhir-pipes
  version. A view row depends on its own resource only, except for the
  partition column, which comes from the lookup table (such as
  `locations`); a change there can move the rows of resources that did not
  change, so the table is reloaded.

The step says which tables it loads in full and why. Only those tables are
analyzed after the commit.

**Privileges.** The schema is created only when it does not exist, which
needs `CREATE` on the database; loading into an existing schema needs
`USAGE` on it and no privilege on the database. Neither mode needs to own
the view tables once they exist:
`SELECT`, `INSERT` and `DELETE` on them are enough (creating or recreating
one needs `CREATE` on the schema, and dropping one its owner, as before).
Upsert also needs `fhir_pipes_loads`: `CREATE` on the schema the first time,
when it makes the table, and then `SELECT`, `INSERT`, `UPDATE` and `DELETE`
on it. The key column's index is made only by the table's owner; for
another role the step says the index is missing, and the deletes scan the
table until the owner adds it. A `replace` load into a schema that has
`fhir_pipes_loads` (an upsert loaded it before) removes the rows of the
tables it loads, so it needs `SELECT` and `DELETE` on that table. Either
mode locks `fhir_pipes_loads` in `ROW EXCLUSIVE` mode (which those
privileges allow) and checks it again under the lock, so it cannot be
renamed or replaced while the load runs. Without
them it stops and names the grant, rather than leave a mark that would let
the next upsert keep the rows it replaced:

```sql
GRANT SELECT, DELETE ON analytics.fhir_pipes_loads TO loader;                   -- replace
GRANT SELECT, INSERT, UPDATE, DELETE ON analytics.fhir_pipes_loads TO loader;   -- upsert
```

A schema that has no `fhir_pipes_loads` is loaded by `replace` exactly as
before.

Changes the log cannot see are not loaded: a file in `raw/` edited by hand
before `--reuse-raw`, or rows changed by hand in a table that keeps its
mark. Run once with `--full` (or `mode = "replace"`) to bring the database
back in line.

On a table of 3,000,000 rows (one Observation view, from files, on an
Apple M4 with PostgreSQL 17 in Docker), a run with 120 changed, deleted or
new resources took 10.0 to 11.5 seconds in the database step with
`replace` and 0.1 seconds with `upsert`; with 10,120 changed, 0.9 seconds.
Comparing the split files to find the changes added about 2.7 seconds to
the file source's split; an incremental search finds them without that.

The `DELETE` leaves each replaced row as a dead tuple until `VACUUM` reclaims
its space. PostgreSQL's autovacuum does that in the background, and is on by
default; keep it on for the schema's tables, or run `VACUUM` on them yourself
(after the refresh, say from a hook) if it is off. After the commit the step
runs `ANALYZE` on each table it loaded, so the planner's statistics describe
the new rows at once rather than after autovacuum's next analyze.

## Settings

Command-line flags win over their environment variables, which win over the
config file (`--config`, else `./fhir-pipes.toml` or `./fhir-warehouse.toml`
when present). Paths in
the file are relative to the file.

| Flag | Environment | Config | Default |
|---|---|---|---|
| `--data` | `FHIR_PIPES_DATA` | `data` | (required) |
| `--profile` | `FHIR_PIPES_PROFILE` | `profile` | the snapshot's copy, else none (base FHIR); a path relative to the config file, or an https URL. See [profiles.md](profiles.md) |
| `--views` | `FHIR_PIPES_VIEWS` | `views` | (required for `views`) |
| `--server` | `FHIR_PIPES_SERVER` | `server.base` | `http://localhost:3447/fhir`; a Cloud Healthcare API store path (`projects/…/fhirStores/…`) is expanded to its FHIR URL |
| `--input` (repeatable) | | `source.files` | no file source: the resources come from the server. See [From files](#from-files) |
| `--server-name` | `FHIR_PIPES_SERVER_NAME` | `server.name` | `hapi-local`; `files` with a file source |
| `--token` | `FHIR_PIPES_TOKEN` | | no token |
| `--token-url` | `FHIR_PIPES_TOKEN_URL` | `server.token_url` | |
| `--token-auth` | `FHIR_PIPES_TOKEN_AUTH` | `server.token_auth` | `post`: the client ID and secret in the token request's form body; `basic` sends them in a Basic `Authorization` header (Aidbox) |
| `--scope` | `FHIR_PIPES_SCOPE` | `server.scope` | no `scope` in the token request |
| | `FHIR_PIPES_CLIENT_ID`, `FHIR_PIPES_CLIENT_SECRET` | | no token |
| `--server-kind` | `FHIR_PIPES_SERVER_KIND` | `server.kind` | `generic`; `hapi` (the same), `medplum` or `aidbox` for those servers' search parameters (both FHIR steps) and Medplum's bulk output files. See [servers.md](servers.md) |
| | | `server.fetch` | `search`; `bulk` for `$export`; `hapi-db` for the HAPI server's database |
| | `FHIR_PIPES_HAPI_DATABASE_URL` | | the HAPI database's connection string, with `fetch = "hapi-db"` |
| | | `server.bulk_timeout` | 14400 (four hours), with `fetch = "bulk"` |
| | | `server.concurrency` | 4: search requests in flight at once in the views step |
| `--username` | `FHIR_PIPES_USERNAME` | `server.username` | no Basic auth |
| | `FHIR_PIPES_PASSWORD` | | |
| `--google-auth` | `FHIR_PIPES_GOOGLE_AUTH=1` | `server.google_auth = true` | no Google auth; `--google-auth=false` or `FHIR_PIPES_GOOGLE_AUTH=0` turns off the file's setting |
| `--countries` | | `tiles.countries` | every country |
| `--threads` | | | number of CPUs; at least 1 |
| `--reuse-raw` | `FHIR_PIPES_REUSE_RAW=true` | | download again; with it, the downloads in `raw/<server>/` and the Location snapshot there are used where present |
| `--full` | | | only what changed (Locations and the views' resource types) |
| `--dry-run` | | | (push only) |

The client ID and secret are read only from the environment, never from flags
or the file. They are traded for a token with the client-credentials grant,
and the token is renewed before it expires, as for the other commands.
`token_auth` and `scope` apply only with a client ID; see the
[README](../README.md#connecting-to-a-fhir-server). With a
username, the password is read only from `FHIR_PIPES_PASSWORD` and the two are
sent as HTTP Basic auth. With `google_auth`, tokens come from Google
Application Default Credentials (`GOOGLE_APPLICATION_CREDENTIALS`, gcloud's
credentials, or the metadata server), as described in the
[README](../README.md#google-cloud-healthcare-api); `--check` shows which.
A token, a client ID, a username and Google auth are mutually exclusive.

```toml
data = "../data"
views = "../ig/fsh-generated/resources"

[server]
base = "https://fhir.example.org/fhir"
name = "prod"
token_url = "https://auth.example.org/realms/x/protocol/openid-connect/token"
# token_auth = "basic"      # the client ID and secret in a Basic header (Aidbox); default "post"
# scope = "system/*.read"   # the scope asked for with the client ID
kind = "generic"            # "medplum" or "aidbox" for those servers; "hapi" is the same as "generic"
fetch = "search"            # "bulk" for a Bulk Data $export, "hapi-db" for HAPI's database
concurrency = 4             # search requests at a time
# google_auth = true        # a Cloud Healthcare API store; base can be projects/…/fhirStores/…

[tools]            # paths or names on the PATH
duckdb = "duckdb"
tippecanoe = "tippecanoe"
rclone = "rclone"

[tables]
strip_prefix = ["icr_"]

[partition]
column = "country"

[[partition.lookup]]        # a view with location_id: country from parquet/locations
key = "location_id"
table = "locations"
table_key = "id"

[[partition.lookup]]        # else campaign_id: from parquet/campaign_calendar
key = "campaign_id"
table = "campaign_calendar"

[tiles]
countries = ["NGA"]

[push]
remote = "r2"               # rclone remote
bucket = "warehouse"
dirs = ["parquet", "tiles", "views"]
files = ["manifest.json"]
exclude = [".DS_Store"]

[catalog]
base_url = "https://data.example.org/warehouse"
extra = "catalog-extra.sql"
spark = true                # also catalog.spark.sql
spark_location = "s3a://warehouse"

[[hooks]]
name = "catalog"
after = "locations"         # only when that step ran; leave out to always run
run = ["./catalog.sh"]      # a path with / is relative to this file

[[hooks]]
name = "alert"
on = "failure"              # "success" (the default), "failure" or "always"
run = ["sh", "-c", "curl -fsS -X POST --data-binary @\"$DATA/last-run.json\" \"$ALERT_URL\""]
```

### Hooks

A hook runs in the config file's directory with `DATA` (the warehouse
directory as an absolute path), `FHIR_BASE` (empty with a file source) and
`SERVER_NAME` set. `on` says when:

- `on = "success"`, the default: in the `hooks` step, after the run's
  tables, manifest and catalog are swapped in, with `STATUS=succeeded` and
  `REFRESHED_AT` (the manifest's time). A hook that fails stops the run,
  which then fails at `hooks`.
- `on = "failure"`: after a run that failed, at any step or before the
  first, once `last-run.json` records the failure, with `STATUS=failed`,
  `FAILED_STEP`, `ERROR` and `EXIT_CODE` as in
  [How the last run went](#how-the-last-run-went). They run before the run
  lets go of the warehouse's lock, so the `last-run.json` they read is this
  run's. A failure hook that fails, or whose program cannot be found, is
  printed as a warning; the run still fails with its own error and exit
  status. Its program is not checked before the run, so a missing alert
  command never stops a run that would succeed.
- `on = "always"`: both. A run whose success hook fails therefore runs an
  `always` hook twice, once in the `hooks` step and once after the failure.

A run that fails at `lock` (another refresh still held the warehouse after
`--lock-timeout`) never wrote `last-run.json`: the file there is the other
run's. Its failure hooks get `FAILED_STEP=lock` and should report from the
environment, not the file.

Failure hooks run only in a run that has the `hooks` step, as success hooks
do: `--only views` or `--skip hooks` runs neither. A config error that stops
the run before the config is read (exit 2) runs no hooks either.

`after` names one of the steps before the hooks, in any case: `locations`,
`views`, `tiles`, `manifest`, `catalog` or `database`. The hook then runs
only when that step ran. It is only for hooks run on success. A config whose
hook names `hooks`, `push` or an unknown step, or has `after` with
`on = "failure"` or `"always"`, is refused before anything runs.

A failure hook can tell someone, or another system, that the refresh failed:
for example post `last-run.json` to a chat webhook, or to an OpenFn webhook
trigger that handles the alert.

### Running under a supervisor

Four environment variables let a program that runs refreshes for you,
such as `fhir-pipes serve`, follow each one:

- `FHIR_PIPES_RESULT_FILE=<path>`: the run also writes its final record,
  the same as `last-run.json`'s, to that path, with an fsync before and
  after the rename. It is written whether or not the run got the
  warehouse's lock, so a run that fails at `lock` or `tools` reports too.
- `FHIR_PIPES_FAILURE_HOOKS=off`: no hook runs after a failure (`on =
  "failure"` or `"always"`); the run prints that they are off. Success
  hooks and `always` hooks in a run that succeeds are unchanged. A
  supervisor that sends its own alert once per run, rather than once per
  attempt, sets it.
- `FHIR_PIPES_RUN_ID=<id>`: given to every hook as `RUN_ID` (empty
  without it), so an integration can tell the attempts of one run from
  separate runs.
- `FHIR_PIPES_SECRET_VARS=<A,B>`: more variables whose values are secrets.
  A refresh always hides the values of `FHIR_PIPES_TOKEN`,
  `FHIR_PIPES_CLIENT_SECRET`, `FHIR_PIPES_PASSWORD`,
  `FHIR_PIPES_HAPI_DATABASE_URL`, `FHIR_PIPES_DATABASE_URL` and
  `FHIR_PIPES_SERVE_TOKEN` in an error before it cuts it short (an HTTP
  error body, `last-run.json`'s `error`); with this, the variables named
  too.

### last-refresh.json

Each run that finishes writes its step timings to
`raw/<server>/last-refresh.json`; a run that stops at an error does not. When
the views step ran, `type_mismatches` lists, per table and column, the values
that did not match the column's type ([Tables](#tables)):

```json
"type_mismatches": {
  "indicator_value": {
    "period_start": {"type": "date", "written_as_date": 12, "written_as_null": 0}
  }
}
```

Tables and columns without any are left out, so `{}` means every value was
written as it is.

The manifest's and `last-refresh.json`'s `server` is the configured server
when the run downloaded from it. A run that downloads nothing (with
`--reuse-raw`, or `--only manifest`) records the server named in
`raw/<server>/.export-state.json`, else in the Location snapshot's
`state.json`, else `null`, rather than a server it did not contact.

## Development

```bash
cargo test --test refresh   # end-to-end runs against tests/fixtures/refresh and a fake FHIR server
cargo clippy --all-targets -- -D warnings
```
