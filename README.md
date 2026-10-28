# fhir-pipes

Data pipelines into and out of FHIR servers, in one binary with no runtime
dependencies.

- **Out of FHIR:** build a SQL-on-FHIR warehouse of Parquet tables from a FHIR
  server, or download resources as NDJSON.
- **Transforms:** turn FHIR resources into datasets for other tools. The first
  is `locations`, which turns the Location registry into GeoParquet for GIS
  tools.
- **Into FHIR:** load resources into a FHIR server with version-checked
  writes, including edits made in a GIS tool and data imported from DHIS2,
  and copy what was downloaded from one FHIR server to another.

It works with HAPI FHIR, Medplum, Aidbox, the Google Cloud Healthcare API and
other FHIR R4 servers; see [Supported FHIR servers](#supported-fhir-servers).

fhir-pipes is intended to move into
[fhir-data-pipes](https://github.com/ohs-foundation/fhir-data-pipes) as a
lighter replacement for its Java pipeline. It brings together two Recode Labs
tools, kiln and fhir-warehouse, and the octofhir/sof SQL-on-FHIR engine. The
reasoning and the plan are in
[docs/notes/2026-10-14-consolidation-with-fhir-data-pipes.md](docs/notes/2026-10-14-consolidation-with-fhir-data-pipes.md).

[ROADMAP.md](ROADMAP.md) lists what each release can do, what is next, and
what was decided against.

## Install

Download the binary for your platform from the
[releases](https://github.com/ohs-foundation/fhir-pipes-rs/releases). The
current release is v0.6.0, with five binaries:

| Platform | File |
|---|---|
| macOS, Apple Silicon | `fhir-pipes-aarch64-apple-darwin` |
| macOS, Intel | `fhir-pipes-x86_64-apple-darwin` |
| Linux, x86_64 (static, no runtime dependencies) | `fhir-pipes-x86_64-unknown-linux-musl` |
| Linux, aarch64 (static, no runtime dependencies) | `fhir-pipes-aarch64-unknown-linux-musl` |
| Windows, x86_64 | `fhir-pipes-x86_64-pc-windows-msvc.exe` |

Each release has a `SHA256SUMS` file to check the download against.

```sh
# macOS on Apple Silicon, for example
curl -LO https://github.com/ohs-foundation/fhir-pipes-rs/releases/latest/download/fhir-pipes-aarch64-apple-darwin
curl -LO https://github.com/ohs-foundation/fhir-pipes-rs/releases/latest/download/SHA256SUMS
shasum -a 256 -c SHA256SUMS --ignore-missing     # sha256sum -c … on Linux
chmod +x fhir-pipes-aarch64-apple-darwin && mv fhir-pipes-aarch64-apple-darwin /usr/local/bin/fhir-pipes
fhir-pipes --version
```

Or build it with Rust:

```sh
cargo install --git https://github.com/ohs-foundation/fhir-pipes-rs --locked fhir-pipes
```

This README describes the main branch. `--server-kind`, `--token-auth` and
`--scope` need v0.6.0 or later.

## Building a SQL-on-FHIR warehouse

`fhir-pipes refresh` downloads the resources a directory of ViewDefinitions
reads, runs the views, and writes each view as a Hive-partitioned Parquet
table, with a manifest of what it wrote. It can also read the resources from
NDJSON and Bundle files instead of a server.

```toml
# fhir-pipes.toml
data = "warehouse"
views = "views"          # a directory of ViewDefinition JSON, such as an IG's fsh-generated/resources

[server]
base = "https://fhir.example.org/fhir"
name = "prod"
```

```sh
fhir-pipes refresh --check                    # settings, steps and tools found
fhir-pipes refresh --only views               # download, run the views, write the tables
fhir-pipes refresh --only views --reuse-raw   # run the views again on the last download
fhir-pipes refresh --input exports/           # from NDJSON and Bundle files, no server
```

The result:

```
warehouse/
  raw/prod/<Type>.ndjson          the downloaded resources
  parquet/<table>/<partition>=<value>/part-0.parquet   one table per view
  views/<name>.json               the views that were run
  manifest.json                   time, server, row count per table, views
```

Query it with anything that reads Parquet, such as DuckDB:

```sql
SELECT * FROM read_parquet('warehouse/parquet/patient_flat/*/*.parquet', hive_partitioning = true);
```

A full refresh can also extract and transform the Location registry, build
map tiles, write a DuckDB catalog (a view per table, which a dashboard can
attach by URL) and Spark SQL tables for a Spark Thrift server, load the view tables into PostgreSQL (all of them each run, or only the rows of the resources that changed), run your own commands afterwards, and push the warehouse to object
storage. [docs/refresh.md](docs/refresh.md) covers the steps, the config file,
reading from files, partitioning and hooks.

### How the views run

The ViewDefinition engine is in [crates/sof](crates/sof), brought in from
[octofhir/sof](https://github.com/octofhir/sof). It evaluates views in memory
on every CPU, reading each resource type once however many views use it, and
uses the [octofhir-fhirpath](https://crates.io/crates/octofhir-fhirpath)
crate for FHIRPath. It passes the official SQL-on-FHIR v2.1 test suite (144
of 144 cases). [crates/sof-lint](crates/sof-lint) checks each view's
structure before anything runs, so a broken view stops the refresh with the
file and the rule it breaks.

The 12 views in fhir-data-pipes (`docker/config/views`) run unchanged, and
give the same rows on its Synthea sample Bundles
(`synthea-hiv/sample_data_segments`) read directly as on the same resources
flattened to NDJSON. `refresh` on the same directory as a file source gives
the same rows again.

## Profiles

An implementation guide's extra Location and Organization columns, admin
units and codes, and the URLs `dhis2` and `population` write, come from a
profile: a TOML file given with `--profile` (a path or an https URL),
`FHIR_PIPES_PROFILE`, or `profile =` in `fhir-pipes.toml`. Without one, only
base FHIR is read. [docs/profiles.md](docs/profiles.md) explains the format.
[examples/profiles/icr.toml](examples/profiles/icr.toml) is ICR's, and
[docs/icr-profile.md](docs/icr-profile.md) runs it on a small piece of ICR
data to show what each part does.

## Commands

| Command | Direction | What it does | Docs |
|---|---|---|---|
| `refresh` | out | Build or refresh a SQL-on-FHIR Parquet warehouse | [refresh.md](docs/refresh.md) |
| `serve` | out | Run refreshes when asked over HTTP or on a schedule, retry failures that may pass, and notify configured destinations (an OpenFn webhook, say) when each run ends; Unix only | [serve.md](docs/serve.md) |
| `views run`, `sql`, `validate`, `lint`, `test` | | Run views over local FHIR files or PostgreSQL, generate their SQL, check them, run SQL-on-FHIR test cases | [below](#running-viewdefinitions-directly) |
| `export` | out | Download every resource of the given types as NDJSON, by search, Bulk Data `$export` or from a HAPI server's database | [below](#downloading-resources-as-ndjson) |
| `extract` | out | Copy Locations and Organizations into a local snapshot, incrementally | [locations.md](docs/locations.md) |
| `transform locations` | transform | Snapshot → partitioned GeoParquet, the first transform | [locations.md](docs/locations.md) |
| `run` | out | `extract` then `transform locations` | [locations.md](docs/locations.md) |
| `inspect` | | Summarise a GeoParquet dataset | [locations.md](docs/locations.md) |
| `diff` | in | Edited GeoJSON or GeoParquet → changed FHIR resources | [locations.md](docs/locations.md) |
| `load` | in | Send NDJSON resources to a FHIR server in transaction bundles, with version checks | [locations.md](docs/locations.md#load) |
| `import location-boundaries` | in | Admin boundaries (GeoJSON, one level) → the admin unit hierarchy as Location NDJSON | [locations.md](docs/locations.md#importing-admin-boundaries-and-sites) |
| `import location-sites` | in | Facilities, schools or settlements (CSV) → site Locations under that hierarchy, with facility Organizations | [locations.md](docs/locations.md#import-location-sites) |
| `sync` | in | Copy what `export` downloaded to a second FHIR server: only what changed, deletions on request | [sync.md](docs/sync.md) |
| `index` | in | Add spatial index identifiers (quadkey, geohash) to Locations | [locations.md](docs/locations.md#index) |
| `population` | in | Population Groups for admin units from a raster such as WorldPop | [locations.md](docs/locations.md#population) |
| `dhis2 locations`, `measures`, `reports` | in | DHIS2 org units, indicators and data → FHIR NDJSON | [dhis2.md](docs/dhis2.md) |
| `libraries build`, `libraries sql` | | SQL on FHIR Libraries: build Library JSON from SQL files, and print the SQL that runs one over a warehouse | [libraries.md](docs/libraries.md) |
| `profile check` | | Compare a profile with its IG's extensions, code systems and profiles | [profiles.md](docs/profiles.md#checking-a-profile-against-its-ig) |
| `terminology valueset`, `conceptmap` | out | A value set or concept map from FHIR packages as a table SQL can join to (offline) | [terminology.md](docs/terminology.md) |

Run `fhir-pipes <command> --help` for every option. NDJSON paths ending in
`.gz` are read and written compressed.

Every command exits 0 on success, 2 for an error you can fix before running
again (a bad flag, a missing or invalid config file, profile, ViewDefinition
or input file), and 1 for a failure while it runs (the network, the server,
the database, a tool such as duckdb or rclone, a hook, a disk write).
`views validate`, `views lint`, `views test` and `profile check` exit 1 when
they find problems.

`refresh` also records how each run went in `last-run.json` in the warehouse
directory (running, succeeded, or failed with the step and the error), can
run hooks after a run that failed (`on = "failure"`), and with
`--lock-timeout` gives up instead of waiting behind another refresh of the
same warehouse. It stops cleanly on SIGTERM or Ctrl-C, and can report to a
program that runs it for you. See
[docs/refresh.md](docs/refresh.md#how-the-last-run-went).

### Running ViewDefinitions directly

`views` works with ViewDefinitions directly, without a warehouse:

```sh
fhir-pipes views run patient_flat.json --input raw/Patient.ndjson          # CSV to stdout
fhir-pipes views run views/ --input raw/ --output parquet --out tables/   # one file per view
fhir-pipes views sql patient_flat.json --dialect duckdb                     # the SQL for a view
fhir-pipes views sql patient_flat.json --ddl --dialect postgres             # a CREATE TABLE for its columns
fhir-pipes views validate patient_flat.json                                 # structure checks
fhir-pipes views validate patient_flat.json --sarif > validate.sarif        # findings for CI code scanning
fhir-pipes views lint patient_flat.json                                     # paths against the FHIR R4/R5 types
fhir-pipes views lint patient_flat.json --shareable                         # also the portable FHIRPath subset
fhir-pipes views test crates/sof/tests/spec                                 # the official test cases
```

`--output parquet` writes typed columns. An integral number such as `5.0` in
an `integer` column is written as the integer; `5.5` is not rounded. With
`--parquet-temporal native`, a dateTime with a time in a `date` column
(`2026-06-15T23:30:00-05:00`) is written as its date part as written
(`2026-06-15`, not converted to UTC first), and partial dates (`2026`,
`2026-06`) are floored to the start of the period. Any other value that does
not fit its column's type is written as null. Each is counted per column and
printed on stderr at the end of the run (`IcrObs.value: 3 values did not fit
integer and were written as null`), so a view whose types do not match the
data can be fixed; the run does not fail. CSV and NDJSON write every value
as it is.

A directory of views is read as `refresh` reads it. An `--input` directory is
read with its subdirectories, skipping hidden files; it reads `.json` and
`.ndjson` files, plain or gzip (`.json.gz`, `.ndjson.gz`). A directory that
holds both `X.ndjson` and `X.ndjson.gz` is refused (exit 2), since both
would be read and every row would appear twice; remove one. A gzip file is
read through every member, so files joined with `cat` work. An NDJSON file,
plain or gzip, is read 20,000 lines at a time, and resources of a type no
view reads are dropped as they are read; only the rows are kept until they
are written.

With `--out FILE.gz`, CSV, NDJSON and JSON are written as gzip. Parquet with
a `.gz` name is refused: Parquet compresses its own pages, and a gzip file
around it is not one that Parquet readers open. A directory of views writes
plain files (`<name>.csv` and so on) into the `--out` directory. Stdin is
read as plain text. In a Bundle, a reference
to another entry by its `urn:uuid` fullUrl, as transaction Bundles from
Synthea have, is rewritten to that entry's `Type/id`, as a FHIR server would
store it, so `getReferenceKey()` and joins work on Bundle files.

`views run --db URL` (or `FHIR_PIPES_DATABASE_URL`) runs against PostgreSQL
holding FHIR resources as JSONB instead of files; it needs the `postgres`
feature, which the release binaries have. `views lint` checks each selector
against the FHIR R4 and R5 core types built into fhir-pipes: unknown elements
(FH01, with a suggestion), a choice element without `ofType()` (FH02), a
complex element, an array or a Reference in a scalar column (FH03–FH05).
Columns under a `forEach` are checked from its items. An element whose type
differs between R4 and R5 is accepted, but not checked further. `views lint
--package hl7.fhir.r4.core --version 4.0.1` checks against a FHIR package
instead, such as an IG's: taken from the local FHIR package cache
(`~/.fhir/packages`) when it is there, otherwise downloaded from
packages.fhir.org. `--package` also takes a package.tgz (a path or an https
URL), an unpacked package or a directory of StructureDefinitions, as
`terminology` and `profile check` do. That needs a
build with the `fhir-packages` feature (see
[Building and testing](#building-and-testing)). `views validate` and
`views lint` also write their findings as JSON (`--json`) or SARIF
(`--sarif`), and `views lint --allow-fn NAME` exempts an engine-specific
function from `--shareable`.

### Downloading resources as NDJSON

`export` downloads every resource of each `--type` to `DIR/<TYPE>.ndjson`:

```sh
fhir-pipes export --server https://fhir.example.org/fhir --type Patient --type Observation --out raw/
```

It pages the search, writes each file beside the target and renames it into
place, so a failed run leaves the previous download intact. When the last page
is full and has no next link, the server's count is checked and a difference
is reported as `count_mismatch`.

`OUT/.export-state.json` records the greatest `meta.lastUpdated` downloaded
for each type. With `--incremental`, a type that has one is searched with
`_lastUpdated=ge<that time>` and the result is merged into its file by id, so
a run fetches only what changed. A `_lastUpdated` search cannot see a deleted
resource, so after the merge the server's count is compared with the file's,
and a type whose counts differ is downloaded in full. `refresh` exports this
way unless given `--full`. The watermark is never later than the server's
clock when the search began (its HTTP `Date`), less five minutes, so a
resource updated while a run is paging is fetched by the next run. `extract`
sets its Location and Organization watermarks the same way.

### Parallel downloads

`--concurrency` (4 by default) is how many search requests `export` keeps in
flight at once, for all types together. Up to that many types download side
by side, and each type's file is the same as when they download one after
another. A full download of a type with more than one page also asks the
server for the type's count (the first page's `total`, else
`_summary=count`). A type with 10,000 resources or more is split into
`_lastUpdated` windows paged side by side:

- The oldest and newest `meta.lastUpdated` come from
  `_sort=_lastUpdated&_count=1` and `_sort=-_lastUpdated&_count=1`. The range
  is cut at whole seconds into windows searched with
  `_lastUpdated=ge<from>&_lastUpdated=lt<to>`. The first window has no lower
  bound and the last no upper bound, so a resource updated during the run is
  still in a window. A resource stamped exactly on a cut is in the window
  that starts there, and only that one.
- Each window is counted. A window holding more than its share is halved, so
  updates bunched in time still spread over several windows. The windows'
  counts must add up to the type's count.
- A window whose paged count is not its count is counted and paged again. A
  second difference is reported as `count_mismatch`.
- The windows are joined oldest first, each in the server's order. A resource
  in two windows (one updated during the run) is kept once, in its later
  version. A windowed file has the same resources as one paged as a single
  stream, in a different order.
- A server that rejects `_sort=_lastUpdated`, gives no count for a window, or
  whose window counts do not add up to the type's is paged as one stream,
  with a line saying why.
- With `--server-kind aidbox`, a window holding more than 50,000 resources
  is halved, and a larger type is split into windows even with
  `--concurrency 1`. With `--server-kind medplum`, the limit is 1,000,000,
  so each window's count is exact. A window is never cut below one second,
  so a second holding more than the limit stays one window. Past the usual
  number of windows, halving stops when a window's halves do not count up
  to it (see [docs/servers.md](docs/servers.md)).

Incremental downloads (`_lastUpdated=ge<watermark>`) stay one stream per
type. The limit covers types and windows together. A `Retry-After` on a 429
or 5xx, or the `RateLimit` wait on a 429, holds back every request, not only
the one that got it.
`--concurrency 1` downloads one type at a time, one page at a time. With
`--bulk`, `--concurrency` is the number of the export's output files
downloaded at once. It does not apply to `--hapi-db`.

The Java pipeline instead pages HAPI in parallel with `_getpagesoffset`.
fhir-pipes does not, since that relies on HAPI's search cache, which
expires, and on behaviour HAPI does not document.

### Copying to a second FHIR server

`sync` copies a directory `export` wrote to another FHIR server (the sink),
as the Java pipeline does with `sinkFhirServerUrl`:

```sh
fhir-pipes export --server $SOURCE --incremental --out raw/ --type Patient --type Encounter
fhir-pipes sync --server $SINK --from raw/ --type Patient --type Encounter
```

It sends transaction Bundles of `PUT <Type>/<id>` without `ifMatch` and
without the source's `meta.versionId` and `meta.lastUpdated`. A record per
sink in `raw/.sync-state.json` and `raw/.sync/` means each run sends only the
resources that are new or whose `meta.lastUpdated` changed. With `--delete`,
resources gone from the download are deleted from the sink, unless that is
more than `--max-delete-share` (a fifth) of a type. `--dry-run` prints the
plan. The auth options are the sink's. See [docs/sync.md](docs/sync.md).

### Bulk Data $export

With `--bulk`, `export` asks the server for a
[Bulk Data](https://hl7.org/fhir/uv/bulkdata/) `$export` instead of paging
searches. The files are the same `OUT/<TYPE>.ndjson`, so `refresh` and the
views read them unchanged.

```sh
fhir-pipes export --bulk --incremental --server https://fhir.example.org/fhir \
  --type Patient --type Observation --out raw/
```

- The kick-off is a system-level `GET [base]/$export?_type=…` with
  `Prefer: respond-async`. With `--incremental`, the types that have a
  watermark are exported together with `_since` the earliest of them and
  merged into their files by id; the types without one are exported in full
  in a second export.
- A kick-off answered with 429 is tried again until `--bulk-timeout`, with
  the waits of a status poll. Aidbox runs one export at a time and answers
  429 while another is running. The kick-off requests, their retries after
  a 5xx or a failed connection, and the waits between them all end by
  `--bulk-timeout`. Nothing is saved in `.export-state.json` until the
  server accepts the kick-off.
- The status URL is polled with the server's `Retry-After`, or a backoff from
  one second up to a minute without one. While the export runs (202), the
  wait is at most 15 seconds, whatever `Retry-After` says: HAPI asks for two
  minutes on every poll. A 429 or 5xx is waited out for as long as its
  `Retry-After` asks (on a 429 without one, its `RateLimit` header's `t=`).
  Each new `X-Progress` is printed.
- The status URL is saved in `.export-state.json` as soon as the server
  returns it. A run that stops before the export is done (a crash, a lost
  connection) polls the same export next time instead of starting another.
  A saved export for other types or another `_since` is cancelled.
- `--bulk-timeout` (four hours by default) is how long a run waits. After
  that it cancels the export with a `DELETE` of the status URL and fails. It
  is also the time allowed for each file's download.
- The output files are downloaded `--concurrency` at a time (4 by default),
  each into a part file. Each type's parts are joined in the manifest's
  order, so the file is the same as one downloaded a file at a time. When a
  download fails, the parts are removed, no type's file or watermark
  changes, and the next run reads the same export again.
- The output files are downloaded with the bearer token only when the
  manifest says `requiresAccessToken`.
- Resources in the manifest's `deleted` Bundles are removed from the files,
  unless the same export also returned them. The server's count is still
  checked after a merge, as for search, because not every server lists
  deletions (HAPI does not); a type whose count differs is exported again in
  full.
- Every type's watermark becomes the manifest's `transactionTime`, or the
  server's clock when it accepted the kick-off (the response's `Date`) when
  that is earlier. Medplum sets `transactionTime` after it has read every
  type, so a resource changed during the export can be older than it; the
  next `_since` must not pass over that change. The `Date` is saved with the
  status URL, so a resumed export uses it too.
- An `OperationOutcome` from the kick-off or the status URL, or an error or
  fatal issue in the manifest's `error` files, fails the run with its text.
  Warnings and information go in the report.
- After the files are read, the status URL gets a `DELETE` so the server can
  remove them.

Only system-level export is supported. `Group/[id]/$export` and
`Patient/$export` are not supported yet.

### Reading a HAPI server's database

With `--hapi-db`, `export` reads the resources from the PostgreSQL database of
a HAPI FHIR JPA server instead of its API, as fhir-data-pipes'
`--fhirFetchMode=HAPI_JDBC` does. It writes the same `OUT/<TYPE>.ndjson`
files, so `refresh` and the views read them unchanged. The connection string
comes only from `FHIR_PIPES_HAPI_DATABASE_URL`, never from a flag or a file:

```sh
export FHIR_PIPES_HAPI_DATABASE_URL='postgres://fhir_pipes_ro:...@hapi-db.example.org:5432/hapi?sslmode=verify-full'
fhir-pipes export --hapi-db --incremental --type Patient --type Observation --out raw/
```

Use `sslmode=verify-full` for a database on another machine: the default,
`prefer`, falls back to plain text, and `require` does not check the
server's certificate. fhir-pipes warns when a remote database is reached
with anything else.

This reads HAPI's internal tables, not a FHIR API, so it ties fhir-pipes to
HAPI's database schema, which HAPI changes between releases. It was tested
with HAPI 8.12 on PostgreSQL 17. Before reading, fhir-pipes checks for the
tables and columns it needs, and refuses a database that lacks them, naming
what it found. Search and `--bulk` do not depend on the server's internals;
use `--hapi-db` when paging the API is too slow for the data, or to see
deletions on a HAPI server without Bulk Data export.

- Each resource is the current version of an `hfj_resource` row that is not
  deleted, read from `hfj_res_ver`. The text is `res_text_vc` (JSON, as HAPI 8
  writes it) or the `res_text` large object (JSON, or gzipped JSON as `JSONC`,
  as older releases wrote it).
- The id comes from `hfj_resource.fhir_id` (HAPI 7 and later). In a database
  without that column, a client-assigned id comes from `hfj_forced_id`, and a
  server-assigned one is the numeric `res_id`.
- HAPI stores a resource without its `id` and `meta`. `meta.versionId` and
  `meta.lastUpdated` come from the row, `meta.source` from
  `hfj_res_ver.source_uri` and `request_id`, and profiles, security labels
  and tags from `hfj_res_tag` and `hfj_tag_def`. Against HAPI 8.12 the lines
  are byte for byte the ones `export` writes from a search.
- `res_updated` has no time zone. HAPI writes it in its JVM's time zone, which
  is UTC in the HAPI Docker images, and fhir-pipes reads it as UTC. On a HAPI
  server running in another time zone, `meta.lastUpdated` is wrong by the
  offset.
- All types are read in one read-only, repeatable-read transaction through a
  cursor, 1000 rows at a time, so a run sees one consistent state of the
  database and memory does not grow with the table. The types are read one
  after another on one connection; `--concurrency` is not used.
- `.export-state.json` keeps a `res_updated` watermark per type under
  `hapi_db`, apart from the search and bulk watermarks. With `--incremental`,
  a run reads the rows updated after the watermark, merges them into the file
  by id, and removes the resources deleted since (a delete sets
  `res_deleted_at`). If the merged file's count then differs from the
  database's, as after an `$expunge`, the type is read in full. Files written
  from a search, a bulk export, another database or another schema are read
  in full the first time.
- The watermark is never later than the database's clock when the run began,
  less five minutes, so a resource HAPI was still writing then is read by the
  next run, as with search.
- The resources of every partition of a partitioned HAPI server go into the
  same files.
- Only PostgreSQL is supported, not the MySQL, MS SQL Server or Oracle
  databases HAPI can also use.

A read-only role for it:

```sql
CREATE ROLE fhir_pipes_ro LOGIN PASSWORD '...';
ALTER ROLE fhir_pipes_ro SET default_transaction_read_only = on;
GRANT CONNECT ON DATABASE hapi TO fhir_pipes_ro;
GRANT USAGE ON SCHEMA public TO fhir_pipes_ro;
GRANT SELECT ON hfj_resource, hfj_res_ver, hfj_res_tag, hfj_tag_def TO fhir_pipes_ro;
GRANT SELECT ON hfj_forced_id TO fhir_pipes_ro;   -- where the table exists
-- Only for resources older HAPI releases stored as large objects; run as a superuser:
GRANT SELECT ON pg_catalog.pg_largeobject TO fhir_pipes_ro;
```

Reading a large object with `lo_get` needs SELECT on that object, which
`pg_read_all_data` does not give, so fhir-pipes reads `pg_largeobject`
directly when the role may. If HAPI's tables are in a schema other than
`public`, add `options=-c%20search_path%3D<schema>` to the connection string,
or set the role's default `search_path`. The state file records the schema
`hfj_resource` was found in, so a run against another schema reads in full.
`refresh` reads this way with `fetch = "hapi-db"` (see
[docs/refresh.md](docs/refresh.md#reading-the-hapi-servers-database)).

### Locations, GIS edits and loading

`extract` copies a server's Locations and Organizations into a local
snapshot, incrementally; `transform locations` turns the snapshot into
partitioned GeoParquet; `run` does both. Edits made in QGIS or another GIS
tool go back with `diff` and `load`:

```sh
fhir-pipes run --server https://fhir.example.org/fhir --snapshot snapshot/ --out out/
fhir-pipes inspect --out out/
fhir-pipes diff --snapshot snapshot/ --in edits.geojson --out changes.ndjson
fhir-pipes load --server https://fhir.example.org/fhir --in changes.ndjson --dry-run
fhir-pipes load --server https://fhir.example.org/fhir --in changes.ndjson
```

`load` sends any NDJSON of FHIR resources, parents first, in transaction
Bundles with `ifMatch` version checks; `--dry-run` prints the plan without
sending anything. `extract` and `run` take `--server-kind` as `export`
does. The commands that make new resources for `load` work offline:
`import location-boundaries` (admin boundaries from GeoJSON),
`import location-sites` (facilities, schools or settlements from CSV),
`index` (spatial index identifiers) and `population` (population Groups
from a raster such as WorldPop):

```sh
fhir-pipes import location-boundaries --profile examples/profiles/icr.toml \
  --in sle_admbnda_adm3.geojson --country "Sierra Leone=SLE" \
  --level province=ADM1_EN:ADM1_PCODE --level district=ADM2_EN:ADM2_PCODE \
  --level chiefdom=ADM3_EN:ADM3_PCODE --out sle-units.ndjson
fhir-pipes population --profile examples/profiles/icr.toml --snapshot snapshot/ \
  --raster sle_ppp_2025.tif --level 2 --year 2025 --out groups.ndjson
```

[docs/locations.md](docs/locations.md) covers the snapshot, the GeoParquet
columns, `diff`, `load`, `import`, `index` and `population`;
[docs/qgis.md](docs/qgis.md) walks through an edit in QGIS.

### Importing from DHIS2

`dhis2 locations`, `dhis2 measures` and `dhis2 reports` read a DHIS2
instance and write org units as Locations, indicators as Measures and their
values as MeasureReports, as NDJSON for `load`. Nothing is written to DHIS2.
The DHIS2 token is read from `FHIR_PIPES_DHIS2_TOKEN` when `--dhis2-token` is
not given. `dhis2` needs a profile with a `[dhis2]` section, which names the
identifier systems and profiles it writes; ICR's is
[examples/profiles/icr.toml](examples/profiles/icr.toml).

```sh
export FHIR_PIPES_PROFILE=examples/profiles/icr.toml
fhir-pipes dhis2 locations --dhis2 https://dhis2.example.org --instance sl-demo --country-code SLE --out locations.ndjson
fhir-pipes dhis2 reports --dhis2 https://dhis2.example.org --instance sl-demo --indicator-group UID \
  --locations locations.ndjson --period LAST_12_MONTHS --level 2 --level 3 --out reports.ndjson
```

[docs/dhis2.md](docs/dhis2.md) has the mapping, incremental runs with
`--previous`, and computing values from raw data values.

### Checking a profile against its IG

`profile check` compares a profile with its implementation guide (a FHIR
package or a directory of JSON resources) and exits 1 when they disagree,
so it can run in the IG's CI:

```sh
fhir-pipes profile check --profile examples/profiles/icr.toml --ig https://icr.healthcampaigns.org/package.tgz
```

Against the published ICR package this currently exits 1, because the package
is missing four items the example profile uses: `dhis2-period`,
`dhis2-indicator`, `report-source` and `icr-report-source-cs`. See
[docs/profiles.md](docs/profiles.md#checking-a-profile-against-its-ig).

## Supported FHIR servers

fhir-pipes reads every server below through its FHIR API, by paged search or
by system-level Bulk Data `$export` (`--bulk`). Only HAPI can also be read
from its database. `--server-kind` tells `export`, `extract`, `run` and
`refresh` which search parameters the server needs; `refresh` also reads it
from `FHIR_PIPES_SERVER_KIND` or `server.kind`.

| Server | Reading | Authentication | `--server-kind` |
|---|---|---|---|
| Any FHIR R4 server | search; `--bulk` where the server has `$export` | none, `--token`, an OAuth client (`--client-id`), or HTTP Basic (`--username`) | `generic` (the default) |
| HAPI FHIR | search; `--bulk` with `hapi.fhir.bulk_export_enabled=true`; `--hapi-db` reads its PostgreSQL database directly | whatever sits in front of it: often HTTP Basic, or an OAuth client through a gateway such as Keycloak; `--hapi-db` uses `FHIR_PIPES_HAPI_DATABASE_URL` | `generic` or `hapi` (the same) |
| Google Cloud Healthcare API | search; `--bulk` uses the store's `$export` | `--google-auth` (Application Default Credentials) | `generic` |
| Medplum | search; `--bulk` | an OAuth client: a ClientApplication with `--client-id`, `--client-secret` and `--token-url <server>/oauth2/token` | `medplum` |
| Aidbox | search, at `<aidbox>/fhir`; `--bulk` once a storage provider is set up | a Client as HTTP Basic (`--username`, `--password`), or as an OAuth client with `--token-auth basic` and `--token-url <aidbox>/auth/token` | `aidbox` |

```sh
export FHIR_PIPES_CLIENT_SECRET=...
fhir-pipes export --server https://api.medplum.com/fhir/R4 --server-kind medplum \
  --client-id "$MEDPLUM_CLIENT_ID" --token-url https://api.medplum.com/oauth2/token \
  --incremental --type Patient --type Observation --out raw/
```

`medplum` asks for `_sort=_lastUpdated` on every page, since Medplum refuses
an `_offset` past 10,000, and reads Medplum's bulk output files with the
token. `aidbox` asks for `_total=none`, so a page does not count the whole
result, and splits a type of more than 50,000 resources into `_lastUpdated`
windows. `generic` and `hapi` add nothing. The kind does not change
authentication or `--hapi-db`. Writing (`load`, `sync`) needs only
transaction Bundles and takes no kind. Aidbox has not been checked against a
real server yet; Medplum was checked against a self-hosted Medplum 5.2.5.

[docs/servers.md](docs/servers.md) has each server's setup: the client and
AccessPolicy for Medplum and Aidbox, rate limits, bulk export storage, and
what each kind changes.

## Connecting to a FHIR server

Any FHIR R4 server that supports standard search with `_lastUpdated`,
`Bundle.link` paging and transaction bundles works. fhir-pipes does not depend
on any vendor's extensions. `export --bulk` also needs the server's
system-level `$export` (in HAPI, `hapi.fhir.bulk_export_enabled=true`).

Medplum and Aidbox need other search parameters: pass `--server-kind medplum`
or `--server-kind aidbox` (in `refresh`, also `FHIR_PIPES_SERVER_KIND` or
`server.kind`; see [Supported FHIR servers](#supported-fhir-servers)).

A server that needs authentication gets one of four kinds:

- `--token` (or `FHIR_PIPES_TOKEN`) is sent as a fixed bearer token. Getting
  it is up to you, and a run that outlasts it fails with a 401.
- `--client-id`, `--client-secret` and `--token-url` (or
  `FHIR_PIPES_CLIENT_ID`, `FHIR_PIPES_CLIENT_SECRET`, `FHIR_PIPES_TOKEN_URL`)
  are an OAuth client, such as a Keycloak client with service accounts
  enabled. fhir-pipes trades them for a token with the client-credentials
  grant before the first request, and for a new one a minute before each token
  expires, so a long run does not outlive its token. A wrong secret or token
  URL fails before any FHIR request. `refresh` reads the client ID and secret
  only from the environment.

  `--token-auth` (or `FHIR_PIPES_TOKEN_AUTH`) says how the client ID and
  secret reach the token endpoint. `post`, the default, puts them in the
  form body (`client_secret_post`). `basic` sends them in a Basic
  `Authorization` header (`client_secret_basic`), each form-urlencoded
  first as RFC 6749 section 2.3.1 says; Aidbox documents only this form.
  `--scope` (or `FHIR_PIPES_SCOPE`) adds a `scope` to the token request,
  such as `system/*.read` for a SMART Backend Services client; without it
  none is sent.
- `--username` and `--password` (or `FHIR_PIPES_USERNAME`,
  `FHIR_PIPES_PASSWORD`) are sent as HTTP Basic auth, as OpenMRS and many
  HAPI servers expect. Use HTTPS: Basic sends the password with every request.
  `refresh` takes the username from `--username` or `server.username`, and
  reads the password only from the environment.
- `--google-auth` (or `FHIR_PIPES_GOOGLE_AUTH=1`) gets tokens from Google
  Application Default Credentials, for a Cloud Healthcare API FHIR store. See
  [Google Cloud Healthcare API](#google-cloud-healthcare-api).

They are mutually exclusive; with none, requests carry no Authorization
header.

```sh
export FHIR_PIPES_CLIENT_SECRET=...          # keep the secret out of shell history
fhir-pipes export --server https://gateway.example.org/fhir \
  --client-id key-me --token-url https://auth.example.org/realms/R/protocol/openid-connect/token \
  --type Patient --out raw/
```

`--timeout` is the total deadline for one HTTP request, 300 seconds by
default. Connection errors, 429 and 5xx responses are retried up to
`--retries` times with exponential backoff and jitter, honouring a
`Retry-After` in seconds or as an HTTP-date, up to 30 seconds. A 429 without
a `Retry-After` is waited out for the `t=` of its `RateLimit` header
(draft-ietf-httpapi-ratelimit-headers), as Medplum sends it, with the same
30-second limit: `RateLimit: "requests";r=0;t=60` waits 30 seconds. With
several policies, in one header or in several `RateLimit` fields (Medplum
sends `"requests"` and `"fhirInteractions"` on every response), the `t=` of
the one whose `r=` is 0 is used, else the longest.
Any other 4xx is final.

Requests ask for gzip (`Accept-Encoding: gzip`); HAPI's search pages are
about fifty times smaller compressed. Searches, counts and the Bulk Data
kick-off and status requests send `Cache-Control: no-cache`, so the server
runs the search again instead of answering from its search cache, which
can return stale results and totals for a minute after a change.

### Google Cloud Healthcare API

A Cloud Healthcare API FHIR store is a FHIR server at
`https://healthcare.googleapis.com/v1/projects/<p>/locations/<l>/datasets/<d>/fhirStores/<s>/fhir`.
`--server` (and `server.base` in `refresh`) also takes the store path,
`projects/<p>/locations/<l>/datasets/<d>/fhirStores/<s>`, and expands it to
that URL.

With `--google-auth`, fhir-pipes finds credentials in the order Google's
client libraries use (Application Default Credentials):

1. The JSON key file named by `GOOGLE_APPLICATION_CREDENTIALS`. For a service
   account key, fhir-pipes signs a JWT with the key (RS256) and trades it at
   the key's `token_uri` for a token with the `cloud-platform` scope. An
   `authorized_user` file's refresh token is traded instead.
2. gcloud's application default credentials,
   `~/.config/gcloud/application_default_credentials.json`
   (`%APPDATA%\gcloud\…` on Windows, or under `$CLOUDSDK_CONFIG`), written by
   `gcloud auth application-default login`.
3. The metadata server on Compute Engine, Cloud Run and GKE, which gives the
   tokens of the attached service account. `GCE_METADATA_HOST` replaces
   `metadata.google.internal`. Off Google Cloud this fails within seconds.

Tokens are renewed a minute before they expire, as with an OAuth client. A
file's `quota_project_id`, or `GOOGLE_CLOUD_QUOTA_PROJECT`, is sent as
`x-goog-user-project`. Tokens and keys are never printed; `refresh --check`
shows only where the credentials come from. Other credential types
(workload identity federation, impersonation) are not supported.

```sh
export GOOGLE_APPLICATION_CREDENTIALS=$HOME/keys/fhir-reader.json
fhir-pipes export --google-auth \
  --server projects/my-project/locations/us-central1/datasets/health/fhirStores/main \
  --type Patient --type Observation --out raw/
```

The account needs an IAM role on the store or its dataset:
`roles/healthcare.fhirResourceReader` to read (`export`, `extract`,
`refresh`), `roles/healthcare.fhirResourceEditor` to write (`load`, `sync`).
`export --bulk` uses the server's `$export`; the Healthcare API's own export
to Cloud Storage is not used.

## Using fhir-pipes with an AI agent

[skills/fhir-pipes](skills/fhir-pipes) is an
[Agent Skill](https://agentskills.io): instructions that tell a coding agent
how to choose and run fhir-pipes commands. It covers servers, auth through
environment variables, incremental and bulk downloads, `refresh` and exit
codes, and has safety rules: secrets only in the environment, `--check` and
`--dry-run` before writing, and no writes to production without the user's
say-so. Copy or symlink the directory into the agent's skills directory:

| Agent | Personal | Project |
|---|---|---|
| Claude Code | `~/.claude/skills/fhir-pipes` | `.claude/skills/fhir-pipes` |
| OpenAI Codex | `~/.agents/skills/fhir-pipes` | `.agents/skills/fhir-pipes` |
| Gemini CLI | `~/.gemini/skills/fhir-pipes` or `~/.agents/skills/fhir-pipes` | `.gemini/skills/fhir-pipes` or `.agents/skills/fhir-pipes` |
| GitHub Copilot in VS Code | `~/.copilot/skills/fhir-pipes` | `.github/skills/fhir-pipes` |

```sh
git clone https://github.com/ohs-foundation/fhir-pipes-rs
mkdir -p ~/.claude/skills
ln -s "$PWD/fhir-pipes-rs/skills/fhir-pipes" ~/.claude/skills/fhir-pipes
```

The skill expects the `fhir-pipes` binary on the `PATH` (see [Install](#install)).

## Not done yet

The open items are in [ROADMAP.md](ROADMAP.md#next). The main ones:

- There is no nested Parquet-on-FHIR output (fhir-data-pipes' SQL-on-FHIR v1
  tables) yet.
- Bulk Data export is system level only: `Group/[id]/$export` and
  `Patient/$export` are not supported.
- Aidbox support has not been checked against a real Aidbox server (see
  [docs/servers.md](docs/servers.md#aidbox)).

## Building and testing

Requires a stable Rust toolchain.

```sh
cargo build --release
./target/release/fhir-pipes --help
cargo test --workspace
```

Two Cargo features add optional parts:

| Feature | Default | Adds |
|---|---|---|
| `postgres` | on, in the release binaries | `views run --db`: views against FHIR resources stored as JSONB in PostgreSQL; `refresh` with `[database]`: the view tables into PostgreSQL. Connections can use TLS (`sslmode=require` and up), through the same rustls as the FHIR client |
| `fhir-packages` | off | `views lint --package`: views checked against a FHIR package, downloaded over HTTP |

```sh
cargo build --release --features fhir-packages     # everything
cargo build --release --no-default-features        # without PostgreSQL
```

`postgres` adds 21 crates and about 1 MB to the release binary;
`fhir-packages` adds over 100 crates. Asking for an option a build lacks says
which feature it needs.

The binary uses [mimalloc](https://github.com/microsoft/mimalloc), compiled
from C source, as its memory allocator in place of the system one.

Release binaries are built in CI for macOS (arm64, x86_64), Linux (x86_64 and
aarch64, statically linked against musl) and Windows (x86_64, with the C
runtime linked statically, set in `.cargo/config.toml`), as single files with
no runtime dependencies. CI tests every PR on Linux, macOS and Windows; the
release builds are slow, so they run for a version tag (which publishes them),
when run by hand from the Actions tab, for a change to `Cargo.toml`,
`crates/*/Cargo.toml`, `Cargo.lock`, `.cargo/config.toml` or the workflows,
and weekly.
`cargo audit` checks `Cargo.lock` against the RustSec advisories on every PR
and weekly; `.cargo/audit.toml` lists what fails the check and the advisories
ignored, each with its reason.

Some tests check output with DuckDB when it is on `PATH` and skip that check
otherwise. DuckDB is never linked into fhir-pipes. The SQL-on-FHIR
conformance suite runs in memory as part of `cargo test`; the generated-SQL
runs need DuckDB, or PostgreSQL through `SOF_CONFORMANCE_DB`.
`tests/export_servers.rs` also runs against a real Medplum or Aidbox server
when `FHIR_PIPES_TEST_MEDPLUM_URL` or `FHIR_PIPES_TEST_AIDBOX_URL` is set
(see [docs/servers.md](docs/servers.md#tests-against-real-servers)).

## Repository layout

```
Cargo.toml        the fhir-pipes binary and the workspace
src/
  main.rs, cli.rs, error.rs
  fs.rs, work.rs, report.rs
                  whole-file writes, the parallel work queue, data-quality issues
  http.rs, http/  the HTTP client: retry, OAuth, Google credentials
  warehouse/      refresh: config, partitioning, manifest, tiles, hooks, push
  views/          views: run, sql, validate, lint, test; running views in memory (run.rs)
  export/         export: search paging, Bulk Data $export (bulk.rs), HAPI database (hapi_db.rs)
  extract/        paging, boundary fetch and cache
  snapshot/       state.json, merge by id
  load/           parents-first ordering, transaction bundles with ifMatch
  sync/           sync: copy an export directory to a second FHIR server
  fhir/           resource parsing, reading resource files, NDJSON reading and writing, FHIR packages
  transform.rs, index/, geometry/, write/, inspect.rs, run.rs
                  the locations transform: hierarchy, geometry, GeoParquet;
                  index/command.rs is the index command (spatial index cells)
  diff/           edits back to FHIR resources
  raster/, population.rs
                  population Groups from a raster
  dhis2/          DHIS2 import
  import/         import location-boundaries and location-sites: GeoJSON and CSV to Locations
  profile/        profiles: loading, and `profile check` against an IG
  terminology/    terminology: value sets and concept maps from FHIR packages as tables
crates/
  sof/            fhir-pipes-sof: the ViewDefinition engine (from octofhir/sof)
  sof-lint/       fhir-pipes-sof-lint: ViewDefinition checks
tests/            integration tests per command; fixtures under tests/fixtures
skills/fhir-pipes/
                  the Agent Skill for AI coding agents
docs/
  refresh.md, servers.md, locations.md, sync.md, dhis2.md, qgis.md, profiles.md, icr-profile.md,
  terminology.md
  notes/          the consolidation notes
  superpowers/    kiln's design specs, plans and spikes
```

## License

Apache-2.0. `crates/sof` and `crates/sof-lint` are MIT OR Apache-2.0, from
octofhir/sof by the OctoFHIR Team.
