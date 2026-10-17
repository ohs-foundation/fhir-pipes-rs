# fhir-pipes compared with the fhir-data-pipes Java pipeline

What the Java pipeline in
[fhir-data-pipes](https://github.com/ohs-foundation/fhir-data-pipes) does, what
fhir-pipes does in each area, and what fhir-pipes would need to replace it for
the sites that run it today.

Compared on 2026-10-15:

- fhir-data-pipes `master` at `613100e` (version `0.2.7-SNAPSHOT`), without
  `experimental/` and the `exp/rust-fhir-pipes` branch, which
  [ROADMAP.md](../../ROADMAP.md#compared-with-the-fhir-data-pipes-rust-rfc)
  covers.
- fhir-pipes `main` at `a79476c`: v0.4.0 plus everything merged since, from
  #22 to #76, to be released as v0.5.0.

First written on 2026-10-15 and updated on 2026-10-16 as the gaps it found
were closed: #36, #37, #39, #40, #41, #42, #43, #45, #46, #47, #48 and #49.
Updated again on 2026-10-16 after a code review of fhir-pipes (issues #51 to
#62) and the fixes merged from it (#63 to #76).

Status in the tables:

| Status | Meaning |
|---|---|
| Same | fhir-pipes does the same thing, perhaps differently |
| Partial | fhir-pipes does some of it |
| Missing | fhir-pipes does not do it |
| Not needed | fhir-pipes covers the need another way, or the Java feature is legacy |

## Summary

fhir-pipes now covers what the Java pipeline does for a site that wants
SQL-on-FHIR view tables: reading FHIR over search (in parallel), Bulk Data
`$export`, a HAPI server's database or files; OAuth, Basic or Google
credentials; incremental runs; running ViewDefinitions (with a more complete
FHIRPath engine); writing Parquet, loading PostgreSQL, describing the tables
for DuckDB and Spark; copying resources to a second FHIR server; and pushing
to object storage.
It does this as one binary instead of Beam, Flink, Spring Boot and a Spark
Thrift server.

The gaps that matter for replacing the Java pipeline at existing sites:

1. **Nested Parquet-on-FHIR tables.** The Java pipeline's base output.
   Existing queries, the Spark Thrift tables and the Superset tutorial read
   them. On hold until a user needs them (ROADMAP).
2. **Running on a schedule with an API.** The Java controller runs
   incremental runs and snapshot purges on cron schedules (full runs are
   started from its web page or API), keeps timestamped snapshots, and has a
   web page, a REST API and Prometheus metrics. fhir-pipes runs from cron or CI
   and keeps one current warehouse.
3. **A Docker image.** Existing sites deploy the Java pipeline as an image and
   compose files.

Not yet checked: OpenMRS FHIR2 over search (#29), the Java pipeline's main
community. Upserting only changed rows into PostgreSQL (#38) is a later
performance step.

Closed since the first version of this comparison:

- **Writing view tables into PostgreSQL** (#25): `refresh` with `[database]`
  replaces every view table in one transaction (#39), and the release
  binaries include it, with TLS (#40). Upserting only changed rows is #38.
- **Bulk Data `$export` as a source** (#27): `export --bulk` and
  `refresh` with `fetch = "bulk"` (#37), with resume, a timeout that cancels
  the export, and `deleted[]` applied.
- **HTTP Basic auth** (#26): `--username` / `--password` on every FHIR
  command (#36).
- **FHIR-to-FHIR sync** (#28): `fhir-pipes sync` copies what `export`
  downloaded to a second FHIR server or a Google Cloud FHIR store (#45).
- **Google Cloud credentials** (#34): `--google-auth` and store paths (#41).
- **Spark table definitions** (#31): `catalog.spark.sql` (#43).
- **An all-or-nothing refresh** (#35): tables staged and swapped in at the
  end (#42).
- **Parallel fetching** (#30): types and `_lastUpdated` windows in parallel
  (#46).
- **The warehouse from files** (#32): `refresh --input` (#47).
- **Reading a HAPI server's database** (#33): `export --hapi-db` (#49).

Changed by the code review (#63 to #76):

- **Stale search results.** fhir-pipes now sends `Cache-Control: no-cache`
  on searches and counts, as the Java pipeline does; without it HAPI could
  return a cached page or total for up to a minute (#64). It also asks for
  gzip, which cut a 14,120-Group download from ICR from about 40 MB to
  0.9 MB.
- **Bulk Data.** After a `202`, the wait before the next status check is
  capped at 15 s, whatever `Retry-After` the server sends; HAPI always sends
  120 s, so a 5,000-Patient export went from 120 s to 30 s. A longer
  `Retry-After` on a `429` or `5xx` is still honoured. Output files download
  in parallel (#69).
- **Allocator.** The release binaries use mimalloc (#67). On Linux, where
  the binaries are built for musl, the ICR views step went from 84.6 s to
  4.9 s.
- **Values that do not fit a column's type.** Before, they were written as
  NULL without a message. Now `5.0` in an integer column is written as `5`,
  a dateTime in a `date` column as its date, and anything else as NULL with a
  count per column printed by the run (#68).
- **Memory.** The views step reads each type in chunks of 20,000 lines
  instead of whole files: the ICR views step went from 2.1 GB to 630 MB peak
  (#70). `load` reads its input in two passes instead of holding it: the ICR
  Location file went from 3.8 GB to 262 MB (#73). `import location-sites` on
  the national GRID3 facility list went from 851 MB to 22 MB (#72).
- **View engine.** Each resource is converted once and every path compiled
  once per view: two ICR and Synthea views used 39–46% less CPU on one
  thread (#71). FHIRPath fixes
  for constants inside strings, `%resource` and `%context`, `ofType` on
  resource types, boundaries on expressions and contained references inside
  `forEach` (#74). Rows are byte-identical to before on the ICR and
  fhir-data-pipes views.
- **Exit codes.** `refresh` and `views` now exit 2 for configuration and
  flag errors, ViewDefinitions that do not compile, and input files that are
  missing or not valid JSON; they exit 1 for failures while running,
  including an input file that exists but cannot be read, as the other
  commands do (#75). `views validate`, `views lint` and `views test` still
  exit 1 when they report findings.

The OpenMRS database mode is not needed: OpenMRS works over FHIR search, which
fhir-pipes supports (to be checked in #29).

fhir-pipes does several things the Java pipeline does not: writing back to
FHIR (GIS edits, DHIS2, population), geospatial outputs, profiles checked
against an IG, a DuckDB catalog, importing admin boundaries and site lists
into FHIR, release binaries for five platforms, and view linting.

## 1. Sources

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| FHIR search | `FHIR_SEARCH` (default). First query with `_total=accurate`, `_count=batchSize` (default 100), `_summary=data`, `Cache-Control: no-cache`; pages fetched in parallel by generating `_getpagesoffset` segments, which only works on HAPI (`FhirSearchUtil`) | `export`, `extract`, `refresh`. Follows `next` links with `_count=1000`, so it works on any server that pages with links; types, and `_lastUpdated` windows of a large type, are fetched in parallel (#46). Sends `Cache-Control: no-cache` and asks for gzip (#64) | Same |
| Resource types | `resourceList` (default `Patient,Encounter,Observation`) | `export --type`; `refresh` downloads the types its views read | Same |
| Incremental fetch | `--since` → `_lastUpdated=ge` (search), `_since` (bulk), `res_updated >` (HAPI JDBC). The controller reads `since` from the last snapshot | Watermark per type in `.export-state.json`, `_lastUpdated=ge`, merged by id. When the first response has an HTTP `Date`, the watermark is never later than that time less five minutes; without one, it is the greatest `meta.lastUpdated` seen | Same |
| Bulk Data `$export` | `BULK_EXPORT`: system-level kick-off with `_type` and `_since`, polls with `Retry-After`, downloads the NDJSON outputs with a plain `URL.openStream()` and no Authorization header (`ReadJsonFn.FromUrl`), so outputs that need the server's credentials fail; keeps `transactionTime` for the next `_since`. Ignores `deleted[]`; no polling timeout | `export --bulk`, `refresh` `fetch = "bulk"` (#37): system-level kick-off with `_type` and `_since`; polls with `Retry-After`, at most 15 s apart after a `202` (#69); `--bulk-timeout` cancels the export with `DELETE`; the status URL is saved, so an interrupted run resumes; outputs downloaded in parallel (`--concurrency`, #69), with the token when `requiresAccessToken`; merged by id; `deleted[]` applied; the count check still runs. No Group or Patient export | Same; fhir-pipes also applies `deleted[]` and resumes |
| HAPI database | `HAPI_JDBC`: reads `hfj_resource`/`hfj_res_ver` in PostgreSQL (or MySQL) directly, in chunks; rebuilds `meta` from the tag tables; the only mode that sees deletions | `export --hapi-db`, `refresh` `fetch = "hapi-db"` (#49): reads `hfj_resource`/`hfj_res_ver` and the tag tables in PostgreSQL, in one read-only transaction streamed by cursor; HAPI 7/8 `fhir_id` and older `hfj_forced_id` layouts; incremental on `res_updated`, deletions included. On HAPI 8.12 the files are byte-identical to search, about 12 times faster. PostgreSQL only; one connection | Same; ties fhir-pipes to HAPI's schema |
| OpenMRS database | `OPENMRS_JDBC`: reads uuids from OpenMRS MySQL tables, then fetches them from the FHIR2 module with `_id=` searches. No incremental mode | None. OpenMRS works through FHIR search (`/openmrs/ws/fhir2/R4`), untested | Missing (search untested) |
| `--activePeriod` | Fetches resources in a date range, then each matched patient's history. Marked in the code as possibly to be removed | None | Not needed |
| JSON Bundle files | `JSON` mode ("experimental"): one Bundle per file, `urn:uuid` references resolved | `views run --input`, and `refresh --input` / `[source] files` to build the warehouse without a server (#22, #47): Bundles, resources and arrays, directories, `urn:uuid` resolved | Same |
| NDJSON files | `NDJSON` mode, Beam file patterns (local, GCS, S3) | `views run --input`, `refresh --input` / `[source] files` (#47); local paths and globs | Same, local files only |
| Parquet input | `PARQUET` mode: reads an existing warehouse back into FHIR resources, to regenerate views or send them to a FHIR server | `refresh --reuse-raw` reruns views on the last download (kept as NDJSON) | Same, for regenerating views |
| Connection check | Calls `metadata` and a `/Patient` count before a run | A wrong token URL or secret fails before any FHIR request | Partial |

## 2. Authentication

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| OAuth client credentials | Token endpoint, client id and secret; token cached and renewed before expiry | Same, renewed a minute before expiry; the secret from `--client-secret` or `FHIR_PIPES_CLIENT_SECRET`, and for `refresh` only from the environment | Same |
| HTTP Basic | `fhirServerUserName` / `fhirServerPassword`, also for the sink server | `--username` / `--password` (`FHIR_PIPES_USERNAME`, `FHIR_PIPES_PASSWORD`) on every FHIR command; `refresh` reads the password only from the environment (#36) | Same |
| Fixed bearer token | None | `--token` / `FHIR_PIPES_TOKEN` | fhir-pipes only |
| Google Cloud credentials | Application Default Credentials for the GCP FHIR store sink | `--google-auth` (#41): Application Default Credentials (service account key, gcloud user credentials, metadata server), renewed before expiry, on every FHIR command, for a source or a sink | Same |
| SMART discovery, renewal on 401 | Neither (TODO in the code) | Neither | Same |

## 3. FHIR versions

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| R4 | Yes (default) | Yes | Same |
| STU3 | Yes: Bunsen schemas, bulk export parameters, controller config | Nothing in fhir-pipes is tested on STU3 data | Missing (untested) |
| R4B, R5 | Only in the ViewDefinition engine (HAPI FHIRPath per version) | The view engine has an R5 test case (`crates/sof/tests/extra/multi_version_r5.json`, an R5 MedicationRequest); nothing else is tested on R4B or R5 data | Same: views only, in both |

## 4. Nested Parquet-on-FHIR

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Nested tables per resource type | FHIR → Bunsen → Avro → Parquet, SQL-on-FHIR v1 schema (`doc/schema.md`): choice types as structs, `<type>Id` fields on references, recursion cut at `recursiveDepth` (default 1) | None. The raw NDJSON is kept per type and DuckDB can query it with `read_json` | Missing, on hold (ROADMAP: waiting for a user) |
| Profiles and extensions in the schema | `structureDefinitionsPath` (US Core bundled): profile extensions become named top-level columns (`birthsex`, `race.ombCategory`) | Profiles do something else: map an IG's Location and Organization extensions to GeoParquet columns | Missing for nested tables |
| File layout | One directory per type, many files (one per worker and flush, `secondsToFlushParquetFiles`), row group size configurable | n/a | — |

## 5. ViewDefinitions

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Engine | HAPI FHIRPath per FHIR version. Runs a copy of the official SQL-on-FHIR v2 tests in CI (`SQLonFHIRv2Test`), skipping 5 cases (`join()` with no separator, date `lowBoundary`/`highBoundary`, one validation case) and with `fn_reference_keys.json` adapted; listed in the spec's implementations registry | `crates/sof` on `octofhir-fhirpath`; passes all 144 cases of the v2.1 suite in memory, with no skips, in CI | Same; fhir-pipes has no skipped cases |
| Constructs | `select`, `column`, `forEach`, `forEachOrNull`, `unionAll`, `where`, `constant`, `getResourceKey()`, `getReferenceKey()` | The same, as the spec's test cases cover them | Same |
| Views to Parquet | `outputParquetViewPath` / `createParquetViews`: one directory per view in each snapshot | `refresh`: one Hive-partitioned table per view (partition looked up through another table), dates and times as Parquet date and timestamp types. A value that does not fit its column's type is converted when nothing is lost (`5.0` → `5`), a dateTime in a `date` column becomes its date, and the rest are NULL and counted per column in the run's output and `last-refresh.json` (#68) | Same, different layout |
| Views to a database | `sinkDbConfigPath`: `CREATE TABLE` per view, rows upserted by resource id (delete then insert), `--recreateSinkTables`. The docs name PostgreSQL and MySQL, but the SQL is PostgreSQL's (`CREATE INDEX IF NOT EXISTS`, `TIMESTAMP WITH TIME ZONE`, `JSONB`, `ON CONFLICT`) with no MySQL variant. Without views, one `(id, datab JSONB)` table per type | `refresh` with `[database]` (#39, #40): every view table, or those listed, replaced in one transaction (`DELETE` then `COPY`); types from the Parquet; tables created, and recreated when their columns change; an index on `id`; an advisory lock so overlapping loads take turns; TLS through `sslmode`. No upsert of only the changed rows (#38) and no raw JSONB mode | Same, replacing rather than upserting |
| Regenerate views without fetching | Controller `VIEWS` run: from the current snapshot into a new `VIEWS_TIMESTAMP_*` directory, sink tables recreated | `refresh --only views --reuse-raw` | Same |
| Try a view on one resource | `POST /test-view` and a web editor | `views run view.json --input resource.json` (CSV to the terminal); `views validate`, `views lint` | Same, without a web page |
| Predefined views | 12 views in `docker/config/views/`, plus Spark SQL versions (`*_flat.sql`) | The same 12 run unchanged; used as an acceptance test | Same |
| View checks | Structural checks when a view is loaded (`ViewDefinition.validateAndSetUp`): resource present, valid view, constant and column names, no empty paths, no duplicate columns, matching `unionAll` branch columns; an `id` column for the database sink | Structural checks before every refresh and in `views validate`; also `views lint` (the portable FHIRPath subset, and `--package` to check paths against a FHIR package) | Same structural checks; fhir-pipes adds linting |

## 6. Incremental runs and deletions

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Incremental merge | Fetch changes into `incremental_run/`, then `ParquetMerger` (Beam/Flink) rewrites the whole warehouse into a new snapshot, latest `meta.lastUpdated` per id winning | Changes merged by id into the per-type NDJSON; every view table is rewritten from the whole download | Same outcome; both rewrite the tables |
| Deletions | Only `HAPI_JDBC` incremental runs see them (tombstones tagged `REMOVE`, also deleted from the sink server and database). Search and bulk export runs keep deleted resources | A type whose count on the server differs from the file's after the merge is downloaded in full, which drops deleted resources; a bulk export also removes the resources its `deleted[]` lists (#37) | fhir-pipes handles more cases; the count check costs a full download of that type |
| Schema changes between runs | The merger needs the same schema; a changed profile needs a full run | Tables are rewritten every run | Not needed |

## 7. Snapshots and retention

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Timestamped snapshots | Every run writes `<dwhRootPrefix>_TIMESTAMP_<time>/`; complete when `timestamp_end.txt` exists; queries go through Hive views that point at the latest | One current warehouse, replaced as a whole at the end of each run (#42); older runs are not kept (versioned run directories are on the ROADMAP) | Missing (by design so far) |
| Retention | `purgeSchedule` and `numOfDwhSnapshotsToRetain`; `DELETE /dwh?snapshotId=` | n/a | Missing |
| Failed runs | `error.log` in the snapshot, downloadable from the controller; the incomplete snapshot is not used | Every table, the manifest and the catalog are staged and swapped in only at the end (#42), so a run that fails before then leaves the warehouse as the last good run left it; `views/` and `tiles/` are staged the same way (#66). Hooks run after the swap: a failing hook does not roll back, and the push does not run | Same |

## 8. Running it: controller, scheduling, monitoring

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Long-running service | Spring Boot controller: one pipeline at a time, embedded Flink | None; a command run from cron, CI or a container's schedule | Missing |
| Schedules | `incrementalSchedule` and `purgeSchedule` (Spring cron). Only incremental runs and purges are scheduled; the first, full run is started from the web page or `POST /run?runMode=FULL`, and no incremental run is scheduled before it (`PipelineManager.checkSchedule`) | External cron | Missing (external) |
| Web page | Run full, incremental or views; create Hive tables; metrics; config; snapshots; view editor. No authentication | None | Missing |
| REST API and CLI | `/run`, `/status`, `/tables`, `/dwh`, `/config`, `/next`, `/download-error-log`; Python `controller` CLI over it | The CLI is the interface; `refresh --check` shows settings and tools | Not needed if run from cron |
| Metrics | Actuator health and Prometheus, Beam counters per resource type (fetched, parse errors, times) | Step timings in `raw/<server>/last-refresh.json`; row counts in `manifest.json`; per-type export reports (`--report`) | Partial |
| Settings | `application.yaml` under `fhirdata:`, overridable by `FHIRDATA_*` variables | `fhir-pipes.toml`, overridable by flags and `FHIR_PIPES_*` variables | Same |

## 9. Execution and scale

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Engine | Apache Beam: DirectRunner, Flink (local or cluster), Dataflow and Spark (Maven profiles). The controller uses local Flink only | One process, all CPUs (rayon); each resource type read once for all its views | Not needed (different design) |
| Memory and speed | Sizing guidance, not a measurement: `docker/.env` gives about 6.7 GB of JVM for 48 cores and 32 MB row groups; the image defaults to `-Xms6g -Xmx6g` | Measured: on the 46,497-resource Synthea sample, the 12 views ran in 0.79 s with 0.87 GB peak (2026-10-14 notes); the ICR warehouse (349k Locations, 164k MeasureReports) rebuilds in about 25 s from data already downloaded. The ICR views step peaks at about 630 MB: resources are read in chunks, and a type's rows are kept until its views finish (#70). Release binaries use mimalloc (#67): on Linux the ICR views step went from 84.6 s to 4.9 s, on macOS from 6.0 s to 4.4 s | Not compared: the two have not been run on the same workload |
| Very large data | Distributed runners | One machine; resources are read in chunks, but each view's rows for a type are kept in memory until that type's tables are written (#70) | Partial: a limit for very large output tables |
| Parallel fetching | Parallel page segments on HAPI | Resource types fetched in parallel, and a large type split into `_lastUpdated` windows paged in parallel, under one shared request limit (#46); works on any server that sorts by `_lastUpdated`. ICR's 350k Locations: 712 s → 302 s | Same, not HAPI-specific |

## 10. Outputs and sinks

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Parquet location | Local, `gs://`, `s3://` written directly (Beam file systems); HDFS with the Spark profile | Local, then `refresh --push` mirrors to any rclone remote (R2, S3, GCS, Azure, …) | Same, through rclone |
| Second FHIR server | `sinkFhirServerUrl`: each page sent as a transaction Bundle of `PUT`s; deletions forwarded in HAPI_JDBC mode; can run with Parquet off for pure FHIR-to-FHIR sync | `fhir-pipes sync` (#45): `PUT` in transaction Bundles without `If-Match`, version fields dropped, only what changed since the last sync to that sink, deletions with `--delete` and a guard against mass deletion. Not a `refresh` step yet | Same |
| Google Cloud FHIR store | `fhirSinkPath` as a store path; Application Default Credentials | A store path (`projects/…/fhirStores/…`) works as the server, with `--google-auth` (#41); `sync` writes to it (#45) | Same |
| Database | See section 5 | See section 5 | Same |
| CSV, BigQuery, Kafka | None | `views run --output csv` | — |

## 11. Query side

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Spark Thrift / Hive | Controller creates `<Resource>_<time>` tables and canonical views per snapshot over JDBC, runs `<Resource>*.sql` files; compose files ship a Spark Thrift server | `[catalog] spark = true` writes `catalog.spark.sql` (#43): a database and an external table per warehouse table, recreated each run, for `spark-sql` or `beeline`; not sent to a Thrift server by fhir-pipes. The Locations table needs Spark 4.1 (its GEOMETRY column); the others work on 3.5 and 4.0 | Same; Locations need Spark 4.1 |
| DuckDB | One notebook (`query/test_duckdb.ipynb`) | Tables queried in place; generated `catalog.sql`, optional `catalog.duckdb` to attach by URL (#23) | fhir-pipes does more |
| Superset | Tutorial over the Thrift server | None | Missing (docs) |
| Notebooks, indicator library, text-to-SQL | `query/` notebooks; `dwh/` HIV/TB indicators (maintenance mode); `query/querygen` (Gemini) | None | Not needed (outside the pipeline) |

## 12. Packaging, tests and docs

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Distribution | Docker image (`us-docker.pkg.dev/cloud-build-fhir/fhir-analytics/main`), jars, compose files for HAPI, OpenMRS, Spark, OpenHIM | Release binaries for macOS, Linux (static musl) and Windows (C runtime linked in, no redistributable needed) with checksums; `cargo install` | Missing: no image or compose files |
| Tests | JUnit and Mockito, including the official SQL-on-FHIR suite (with 5 skips); end-to-end on Cloud Build against HAPI, OpenMRS, bulk export, controller with Spark, FHIR-to-FHIR sync | Tests with default features on Linux, macOS and Windows for every PR (933 on Linux, 920 on Windows, where some tests are Unix-only), including the official SQL-on-FHIR suite and fake FHIR servers; on Linux also 953 with all features and the database tests against PostgreSQL over TLS; `cargo audit` on every PR and weekly (#65); bulk export, `sync` and the HAPI database source were checked by hand against HAPI 8.12, not in CI; nothing against OpenMRS yet (#29) | Partial |
| Docs | mkdocs site: concepts, installation (Docker, controller, pipeline), tutorials (single machine, test servers, Superset, GCP), schema | README, ROADMAP, `docs/` per command, profile walkthrough | Partial: no install-with-Docker or dashboard tutorials |
| Licence | Apache-2.0, Google CLA | Apache-2.0 (sof crates MIT OR Apache-2.0) | Same; CLA needed to contribute |

## 13. What fhir-pipes does that fhir-data-pipes does not

- Importing a Location registry: `import location-boundaries` (admin
  boundary files → the admin hierarchy) and `import location-sites` (facility,
  school or settlement lists → site Locations under it), ported from kiln
  (#48).
- Writing into FHIR: `diff` (GIS edits back to resources), `load`
  (version-checked transaction Bundles, parents first), `index`, `population`,
  `dhis2 locations`, `measures`, `reports`.
- Geospatial outputs: the Location registry as GeoParquet with the admin
  hierarchy, PMTiles map tiles, QGIS round trip.
- Profiles for an IG, and `profile check` against the IG.
- View linting (the portable FHIRPath subset, paths checked against a FHIR
  package), and the official SQL-on-FHIR suite passed without skips.
- Hive-partitioned view tables with a partition looked up through another
  table.
- A DuckDB catalog of the warehouse.
- Bearer-token auth, retries with backoff and `Retry-After`, and a check of the server's count after incremental downloads.
- Bulk Data `$export` that applies `deleted[]`, resumes an interrupted export, and cancels one that runs too long.
- The view tables loaded into PostgreSQL in one transaction, so readers never see a mix of runs.

## Potential features

Grouped by how much they matter for moving existing fhir-data-pipes sites
over. Each has an issue where one is filed; the rest are candidates for
ROADMAP once agreed.

### Needed to replace the Java pipeline at existing sites

| Feature | Why | Size | Issue |
|---|---|---|---|
| Write view tables into PostgreSQL | The single-machine setup and its dashboards read view tables from PostgreSQL. Upsert by resource id, as the Java sink does, or replace each table | Medium | #25, done (#39, #40); upsert is #38 |
| HTTP Basic auth | OpenMRS and many HAPI deployments use it | Small | #26, done (#36) |
| Bulk Data `$export` source | Second fetch mode in the Java pipeline; faster for large servers that support it. Use `deleted[]`, which the Java pipeline ignores | Medium | #27, done (#37); faster polling and parallel downloads (#69) |
| Copy to a second FHIR server | FHIR-to-FHIR sync (Shared Health Record use case): `load` without `If-Match`, or a `sync` command; forward deletions | Small to medium | #28, done (#45) |
| Docker image and a compose example | How existing sites deploy | Small |  |
| Nested Parquet-on-FHIR | Existing queries and Spark tables read it. On hold until a site needs it | Large |  |
| Test against OpenMRS FHIR2 | The main community; it caps `_count` at 100 and has quirks the Java pipeline works around | Small | #29 (later) |

### Useful, not blocking

| Feature | Why | Size | Issue |
|---|---|---|---|
| Parallel page fetching | Faster first loads on big servers. HAPI's `_getpagesoffset` as the Java pipeline does it, or several types at once | Medium | #30, done (#46) |
| Spark or Hive table definitions | Sites with a Spark Thrift server could keep it: write a `CREATE TABLE … USING PARQUET` file next to `catalog.sql` | Small | #31, done (#43) |
| Make a refresh all-or-nothing | A failed run can leave tables from two runs (section 7). Stage every table and swap them in at the end | Small | #35, done (#42) |
| Snapshots and retention | Lets a run be rolled back and queries read a consistent set. Could be timestamped directories with the catalog pointing at the latest | Medium | option 2 of #35, on the ROADMAP |
| A schedule and status endpoint | For sites that want a long-running service instead of cron: a `serve` mode with a schedule, `/status`, `/run` and Prometheus metrics | Medium to large |  |
| `refresh` from files | Run the warehouse from NDJSON or Bundle files (exports, Synthea) without a server | Small | #32, done (#47) |
| STU3 | Only if a site still runs STU3 | Medium |  |
| HAPI database source | Faster than search for very large HAPI servers, and sees deletions. Ties fhir-pipes to HAPI's schema | Medium | #33, done (#49) |
| Google Cloud FHIR store sink | Only for GCP sites | Small to medium | #34, done (#41, #45) |

### Not needed

- Beam, Flink, Dataflow and Spark runners: fhir-pipes runs as one process.
- `--activePeriod`: the Java code itself marks it for removal.
- OpenMRS database reads: FHIR search covers it, once tested.
- The `dwh/` Python library, notebooks and `querygen`: outside the pipeline.
- OpenHIM, Info Gateway: no code in the Java pipeline either.

## Questions for review

1. Which existing fhir-data-pipes sites are we aiming at first (OpenMRS
   deployments, HAPI with PostgreSQL, GCP), and what do their queries read:
   nested tables, view tables in Parquet, or view tables in PostgreSQL?
2. Is running from cron or CI acceptable to them, or do they need the
   controller's web page, API and schedules?
3. Should a run keep snapshots, or is one current warehouse with the previous
   tables kept until the new ones are written enough?
4. For the PostgreSQL sink: upsert changed rows (as the Java pipeline does) or
   replace whole tables each run?
5. Is Basic auth plus OAuth enough, or do any sites need SMART discovery?
