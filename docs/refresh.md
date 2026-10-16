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

### Steps

| Step | What it does | Needs |
|---|---|---|
| `locations` | the Location registry, as `fhir-pipes run` does it: extracted incrementally into `raw/<server>/locations`, transformed into `parquet/locations` | |
| `views` | download each resource type the views read, as `fhir-pipes export --incremental` does (only what changed since the last refresh; by search, or by `$export` with `fetch = "bulk"`), run each type's views on one read of it, write each view's table | |
| `tiles` | the registry → `tiles/admin.pmtiles`, `facilities`, `settlements`, `catchments` | duckdb (spatial), tippecanoe |
| `manifest` | `manifest.json`: time, server, each table's row count, the views, the Location extract's state | |
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
views,tiles` picks steps (the manifest, the catalog and the database are added
when a data step is picked), `--skip tiles` leaves some out,
and `--push` adds the push. Before anything runs, the binary checks that each
tool the chosen steps need is installed, and stops if one is missing.

### What a failed run leaves behind

A run does not change the warehouse's tables, `manifest.json` or `catalog.sql`
until every step up to and including `database` has succeeded. Until then it
writes them to staging directories:

| Staged at | Swapped in to |
|---|---|
| `parquet/.staging/<table>/`, for each table the run writes, `locations` included | `parquet/<table>/` |
| `parquet/.staging/_report.json` (the Location transform's report) | `parquet/_report.json` |
| `.staging/manifest.json`, `.staging/catalog.sql` | `manifest.json`, `catalog.sql` |

While the run goes on, each step reads a table from staging if the run wrote
it, and from `parquet/` if not: the views look partitions up in the new
`locations` and in lookup tables written earlier in the run, the tiles are
built from the new `locations`, and the manifest, the catalog and the database
step describe or load the tables as they will be after the swap. Tables the
run does not write (`locations` with `--only views`, say) stay as they are and
are counted and listed as before.

After the `database` step, each staged table is renamed into `parquet/` (the
old one is renamed aside and then removed), then `manifest.json` and
`catalog.sql` are renamed into place. A reader can see tables from two runs
only during those renames. A table is never half-written.

So a run that fails in `locations`, `views`, `tiles`, `manifest`, `catalog`
or `database` leaves every table, `manifest.json` and `catalog.sql` exactly as
the previous successful run left them, and removes its staging. What it does
change:

- `raw/`: the downloads and the extract state move forward, so the next run
  downloads only what changed since and rebuilds every table from the whole
  download.
- `views/`: the copies of the views are written by the `views` step.
- `tiles/`: the archives are written by the `tiles` step. They are not staged.
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

A run that is killed leaves its staging behind; the next run removes it
before it starts. A run killed between the two renames of a table's swap
leaves the old table as `parquet/.<table>.old`; the next run renames it
back. While it runs, a refresh holds a lock on `.refresh.lock` in the
warehouse directory, so a second refresh of the same warehouse waits for the
first to finish.

Disk use doubles for the tables a run writes, until the swap.

### Tables

Each ViewDefinition writes `parquet/<table>/<column>=<value>/part-0.parquet`.
The table is the view's name in snake case minus a configured prefix
(`IcrIndicatorValue` → `indicator_value`). Dates, dateTimes, instants and times
are parquet date and timestamp types, not text.

Every row gets a partition column (default `country`), looked up through
another table: the first `[[partition.lookup]]` rule whose `key` column the view
has gives the table and column to look the value up in. A row without a match
goes to the null partition, `__HIVE_DEFAULT_PARTITION__`, as hive and DuckDB
name it. A view with no rows writes one empty file there, so the table keeps
its columns. A view with a column of the partition column's name stops the
run, since that column would be lost beside the partition column: set
`[partition] column` to another name (the fhir-data-pipes views have an
address `country` column, for one). The `locations` table stays partitioned
by `country` whatever the view tables use. The manifest, the catalog, the
database step and the partition lookups read each table by its own
first-level partition directories, so a lookup through `locations` gives the
Location's country as the row's partition value either way. Tables other views look up in are written first, and are read
back from staging. Tables are swapped in at the end of the run (see [What a
failed run leaves behind](#what-a-failed-run-leaves-behind)).

The views directory can be any directory of JSON files: files whose
`resourceType` is `ViewDefinition`, or ends in `/ViewDefinition` as in an IG
build (`fsh-generated/resources/Binary-*.json`), are views; the rest are
skipped. Each view is checked against the spec's structure rules before
anything runs, and a plain copy is saved to `views/<name>.json`.

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
views and `--reuse-raw` work the same way. The export's `transactionTime` is
the next refresh's `_since`, and the resources the export lists as deleted
are removed. A refresh that stops while the server is still exporting polls
the same export next time. `--full` exports every type without `_since`.
The `locations` step still uses search. `Group/[id]/$export` and
`Patient/$export` are not supported yet.

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
joins work. A glob pattern reads the `.json` and `.ndjson` files and the
directories it matches, in path order. A path that does not exist, or a
pattern that matches nothing, stops the run.

The steps then run as for a server:

- The resources are split by type into `raw/<name>/<Type>.ndjson`, each file
  written beside its target and renamed into place. `name` is
  `--server-name` or `server.name`, `files` by default. A resource that
  appears more than once (same type and id) is kept once: the copy with the
  latest `meta.lastUpdated`, or, on a tie or when neither has one, the copy
  read last (the later file in path order). A `<Type>.ndjson` from an earlier
  run, of a type the files no longer have, is removed.
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

Every run reloads every table. On the ICR warehouse (10 view tables, 229,082
rows, 161,216 in the largest) that takes about half a second.

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
| | `FHIR_PIPES_CLIENT_ID`, `FHIR_PIPES_CLIENT_SECRET` | | no token |
| | | `server.fetch` | `search`; `bulk` for `$export` |
| | | `server.bulk_timeout` | 14400 (four hours), with `fetch = "bulk"` |
| `--username` | `FHIR_PIPES_USERNAME` | `server.username` | no Basic auth |
| | `FHIR_PIPES_PASSWORD` | | |
| `--google-auth` | `FHIR_PIPES_GOOGLE_AUTH=1` | `server.google_auth = true` | no Google auth; `--google-auth=false` or `FHIR_PIPES_GOOGLE_AUTH=0` turns off the file's setting |
| `--countries` | | `tiles.countries` | every country |
| `--threads` | | | number of CPUs |
| `--reuse-raw` | `FHIR_PIPES_REUSE_RAW=true` | | download again |
| `--full` | | | only what changed (Locations and the views' resource types) |
| `--dry-run` | | | (push only) |

The client ID and secret are read only from the environment, never from flags
or the file. They are traded for a token with the client-credentials grant,
and the token is renewed before it expires, as for the other commands. With a
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
fetch = "search"            # or "bulk" for a Bulk Data $export
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
```

Hooks run in the config file's directory with `DATA`, `FHIR_BASE` (empty with
a file source), `SERVER_NAME` and `REFRESHED_AT` (the manifest's time) set,
after the run's tables, manifest and catalog are swapped in.

Each run writes its step timings to `raw/<server>/last-refresh.json`.

## Development

```bash
cargo test --test refresh   # end-to-end runs against tests/fixtures/refresh and a fake FHIR server
cargo clippy --all-targets -- -D warnings
```
