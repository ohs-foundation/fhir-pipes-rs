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
- fhir-pipes `main` at `f4b2d59` (v0.4.0, plus the Bundle and catalog
  changes since).

Status in the tables:

| Status | Meaning |
|---|---|
| Same | fhir-pipes does the same thing, perhaps differently |
| Partial | fhir-pipes does some of it |
| Missing | fhir-pipes does not do it |
| Not needed | fhir-pipes covers the need another way, or the Java feature is legacy |

## Summary

fhir-pipes already covers the core of the Java pipeline for a site that reads
FHIR over search and wants SQL-on-FHIR view tables in Parquet: fetching with
search and OAuth, incremental runs, running ViewDefinitions (with a more
complete FHIRPath engine), writing Parquet, and pushing it to object storage.
It does this as one binary instead of Beam, Flink, Spring Boot and a Spark
Thrift server.

The gaps that matter for replacing the Java pipeline at existing sites:

1. **Nested Parquet-on-FHIR tables.** The Java pipeline's base output.
   Existing queries, the Spark Thrift tables and the Superset tutorial read
   them. On hold until a user needs them (ROADMAP).
2. **Writing view tables into PostgreSQL.** The Java pipeline upserts view
   rows into a sink database (`sinkDbConfigPath`), which the single-machine
   tutorial and dashboards use.
3. **Bulk Data `$export` as a source.** The Java pipeline's second fetch mode,
   and the one its docs expect to become the main one.
4. **HTTP Basic auth.** The Java pipeline's default for OpenMRS and HAPI test
   servers; fhir-pipes has only bearer tokens and OAuth client credentials.
5. **FHIR-to-FHIR sync.** The Java pipeline can copy resources to a second
   FHIR server or a Google Cloud FHIR store. fhir-pipes' `load` checks
   versions against the same server, so it cannot copy to another.
6. **Running on a schedule with an API.** The Java controller runs
   incremental runs and snapshot purges on cron schedules (full runs are
   started from its web page or API), keeps timestamped snapshots, and has a
   web page, a REST API and Prometheus metrics. fhir-pipes runs from cron or CI
   and keeps one current warehouse.
7. **A Docker image.** Existing sites deploy the Java pipeline as an image and
   compose files.

Sources specific to HAPI and OpenMRS (reading their databases directly) are a
smaller gap: both servers work over FHIR search, which fhir-pipes supports.

fhir-pipes does several things the Java pipeline does not: writing back to
FHIR (GIS edits, DHIS2, population), geospatial outputs, profiles checked
against an IG, a DuckDB catalog, release binaries for five platforms, and view
linting.

## 1. Sources

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| FHIR search | `FHIR_SEARCH` (default). First query with `_total=accurate`, `_count=batchSize` (default 100), `_summary=data`; pages fetched in parallel by generating `_getpagesoffset` segments, which only works on HAPI (`FhirSearchUtil`) | `export`, `extract`, `refresh`. Follows `next` links with `_count=1000`, so it works on any server that pages with links; pages of one type are fetched one after another | Same, slower on large servers |
| Resource types | `resourceList` (default `Patient,Encounter,Observation`) | `export --type`; `refresh` downloads the types its views read | Same |
| Incremental fetch | `--since` → `_lastUpdated=ge` (search), `_since` (bulk), `res_updated >` (HAPI JDBC). The controller reads `since` from the last snapshot | Watermark per type in `.export-state.json`, `_lastUpdated=ge`, merged by id. When the first response has an HTTP `Date`, the watermark is never later than that time less five minutes; without one, it is the greatest `meta.lastUpdated` seen | Same |
| Bulk Data `$export` | `BULK_EXPORT`: system-level kick-off with `_type` and `_since`, polls with `Retry-After`, downloads the NDJSON outputs with a plain `URL.openStream()` and no Authorization header (`ReadJsonFn.FromUrl`), so outputs that need the server's credentials fail; keeps `transactionTime` for the next `_since`. Ignores `deleted[]`; no polling timeout | None | Missing |
| HAPI database | `HAPI_JDBC`: reads `hfj_resource`/`hfj_res_ver` in PostgreSQL (or MySQL) directly, in chunks; rebuilds `meta` from the tag tables; the only mode that sees deletions | None | Missing |
| OpenMRS database | `OPENMRS_JDBC`: reads uuids from OpenMRS MySQL tables, then fetches them from the FHIR2 module with `_id=` searches. No incremental mode | None. OpenMRS works through FHIR search (`/openmrs/ws/fhir2/R4`), untested | Missing (search untested) |
| `--activePeriod` | Fetches resources in a date range, then each matched patient's history. Marked in the code as possibly to be removed | None | Not needed |
| JSON Bundle files | `JSON` mode ("experimental"): one Bundle per file, `urn:uuid` references resolved | `views run --input`: Bundles, resources and arrays, subdirectories, `urn:uuid` resolved (#22). `refresh` reads only from a server | Partial: files work for `views`, not `refresh` |
| NDJSON files | `NDJSON` mode, Beam file patterns (local, GCS, S3) | `views run --input`, local files | Partial (no object storage, not in `refresh`) |
| Parquet input | `PARQUET` mode: reads an existing warehouse back into FHIR resources, to regenerate views or send them to a FHIR server | `refresh --reuse-raw` reruns views on the last download (kept as NDJSON) | Same, for regenerating views |
| Connection check | Calls `metadata` and a `/Patient` count before a run | A wrong token URL or secret fails before any FHIR request | Partial |

## 2. Authentication

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| OAuth client credentials | Token endpoint, client id and secret; token cached and renewed before expiry | Same, renewed a minute before expiry; the secret only from the environment | Same |
| HTTP Basic | `fhirServerUserName` / `fhirServerPassword`, also for the sink server | None | Missing |
| Fixed bearer token | None | `--token` / `FHIR_PIPES_TOKEN` | fhir-pipes only |
| Google Cloud credentials | Application Default Credentials for the GCP FHIR store sink | None | Missing (only needed with that sink) |
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
| Views to Parquet | `outputParquetViewPath` / `createParquetViews`: one directory per view in each snapshot | `refresh`: one Hive-partitioned table per view (partition looked up through another table), dates and times as Parquet date and timestamp types | Same, different layout |
| Views to a database | `sinkDbConfigPath`: `CREATE TABLE` per view, rows upserted by resource id (delete then insert), `--recreateSinkTables`. The docs name PostgreSQL and MySQL, but the SQL is PostgreSQL's (`CREATE INDEX IF NOT EXISTS`, `TIMESTAMP WITH TIME ZONE`, `JSONB`, `ON CONFLICT`) with no MySQL variant. Without views, one `(id, datab JSONB)` table per type | `views sql --ddl` writes `CREATE TABLE` (ANSI, PostgreSQL, DuckDB types); `views run --db` reads FHIR from PostgreSQL JSONB. Nothing writes rows to a database | Missing |
| Regenerate views without fetching | Controller `VIEWS` run: from the current snapshot into a new `VIEWS_TIMESTAMP_*` directory, sink tables recreated | `refresh --only views --reuse-raw` | Same |
| Try a view on one resource | `POST /test-view` and a web editor | `views run view.json --input resource.json` (CSV to the terminal); `views validate`, `views lint` | Same, without a web page |
| Predefined views | 12 views in `docker/config/views/`, plus Spark SQL versions (`*_flat.sql`) | The same 12 run unchanged; used as an acceptance test | Same |
| View checks | Structural checks when a view is loaded (`ViewDefinition.validateAndSetUp`): resource present, valid view, constant and column names, no empty paths, no duplicate columns, matching `unionAll` branch columns; an `id` column for the database sink | Structural checks before every refresh and in `views validate`; also `views lint` (the portable FHIRPath subset, and `--package` to check paths against a FHIR package) | Same structural checks; fhir-pipes adds linting |

## 6. Incremental runs and deletions

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Incremental merge | Fetch changes into `incremental_run/`, then `ParquetMerger` (Beam/Flink) rewrites the whole warehouse into a new snapshot, latest `meta.lastUpdated` per id winning | Changes merged by id into the per-type NDJSON; every view table is rewritten from the whole download | Same outcome; both rewrite the tables |
| Deletions | Only `HAPI_JDBC` incremental runs see them (tombstones tagged `REMOVE`, also deleted from the sink server and database). Search and bulk export runs keep deleted resources | A type whose count on the server differs from the file's after the merge is downloaded in full, which drops deleted resources | fhir-pipes handles more cases; costs a full download of that type |
| Schema changes between runs | The merger needs the same schema; a changed profile needs a full run | Tables are rewritten every run | Not needed |

## 7. Snapshots and retention

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Timestamped snapshots | Every run writes `<dwhRootPrefix>_TIMESTAMP_<time>/`; complete when `timestamp_end.txt` exists; queries go through Hive views that point at the latest | One current warehouse, updated table by table (see Failed runs); `manifest.json` records the run | Missing (by design so far) |
| Retention | `purgeSchedule` and `numOfDwhSnapshotsToRetain`; `DELETE /dwh?snapshotId=` | n/a | Missing |
| Failed runs | `error.log` in the snapshot, downloadable from the controller; the incomplete snapshot is not used | Exit code and stderr. Each table is replaced only once its new files are written, but tables are replaced one at a time: a run that fails after some tables (or in the catalog, hooks or push) leaves those tables updated and the rest from the previous run | Partial |

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
| Memory | Sizing guidance, not a measurement: `docker/.env` gives about 6.7 GB of JVM for 48 cores and 32 MB row groups; the image defaults to `-Xms6g -Xmx6g` | Measured: on the 46,497-resource Synthea sample, the 12 views ran in 0.79 s with 0.87 GB peak (2026-10-14 notes); the ICR warehouse (349k Locations, 164k MeasureReports) rebuilds in about 25 s from data already downloaded | Not compared: the two have not been run on the same workload |
| Very large data | Distributed runners | One machine; each type's resources are held in memory while its views run | Partial: a limit for very large types |
| Parallel fetching | Parallel page segments on HAPI | Sequential pages | Missing (performance) |

## 10. Outputs and sinks

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Parquet location | Local, `gs://`, `s3://` written directly (Beam file systems); HDFS with the Spark profile | Local, then `refresh --push` mirrors to any rclone remote (R2, S3, GCS, Azure, …) | Same, through rclone |
| Second FHIR server | `sinkFhirServerUrl`: each page sent as a transaction Bundle of `PUT`s; deletions forwarded in HAPI_JDBC mode; can run with Parquet off for pure FHIR-to-FHIR sync | `load` sends transaction Bundles, but with `If-Match` from the source's `meta.versionId`, which only matches on the same server | Missing (needs a copy mode without version checks) |
| Google Cloud FHIR store | `fhirSinkPath` as a store path; Application Default Credentials | None | Missing |
| Database | See section 5 | See section 5 | Missing |
| CSV, BigQuery, Kafka | None | `views run --output csv` | — |

## 11. Query side

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Spark Thrift / Hive | Controller creates `<Resource>_<time>` tables and canonical views per snapshot over JDBC, runs `<Resource>*.sql` files; compose files ship a Spark Thrift server | None. `catalog.sql` / `catalog.duckdb` give DuckDB a view per table instead | Partial: no Spark tables |
| DuckDB | One notebook (`query/test_duckdb.ipynb`) | Tables queried in place; generated `catalog.sql`, optional `catalog.duckdb` to attach by URL (#23) | fhir-pipes does more |
| Superset | Tutorial over the Thrift server | None | Missing (docs) |
| Notebooks, indicator library, text-to-SQL | `query/` notebooks; `dwh/` HIV/TB indicators (maintenance mode); `query/querygen` (Gemini) | None | Not needed (outside the pipeline) |

## 12. Packaging, tests and docs

| Feature | fhir-data-pipes | fhir-pipes | Status |
|---|---|---|---|
| Distribution | Docker image (`us-docker.pkg.dev/cloud-build-fhir/fhir-analytics/main`), jars, compose files for HAPI, OpenMRS, Spark, OpenHIM | Release binaries for macOS, Linux and Windows with checksums; `cargo install` | Missing: no image or compose files |
| Tests | JUnit and Mockito, including the official SQL-on-FHIR suite (with 5 skips); end-to-end on Cloud Build against HAPI, OpenMRS, bulk export, controller with Spark, FHIR-to-FHIR sync | 639 tests on Linux, macOS and Windows for every PR, including the official SQL-on-FHIR suite and a fake FHIR server; no end-to-end tests against a real HAPI or OpenMRS | Partial |
| Docs | mkdocs site: concepts, installation (Docker, controller, pipeline), tutorials (single machine, test servers, Superset, GCP), schema | README, ROADMAP, `docs/` per command, profile walkthrough | Partial: no install-with-Docker or dashboard tutorials |
| Licence | Apache-2.0, Google CLA | Apache-2.0 (sof crates MIT OR Apache-2.0) | Same; CLA needed to contribute |

## 13. What fhir-pipes does that fhir-data-pipes does not

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

## Potential features

Grouped by how much they matter for moving existing fhir-data-pipes sites
over. Each is a candidate for ROADMAP once agreed.

### Needed to replace the Java pipeline at existing sites

| Feature | Why | Size |
|---|---|---|
| Write view tables into PostgreSQL | The single-machine setup and its dashboards read view tables from PostgreSQL. Upsert by resource id, as the Java sink does, or replace each table | Medium |
| HTTP Basic auth | OpenMRS and many HAPI deployments use it | Small |
| Bulk Data `$export` source | Second fetch mode in the Java pipeline; faster for large servers that support it. Use `deleted[]`, which the Java pipeline ignores | Medium |
| Copy to a second FHIR server | FHIR-to-FHIR sync (Shared Health Record use case): `load` without `If-Match`, or a `sync` command; forward deletions | Small to medium |
| Docker image and a compose example | How existing sites deploy | Small |
| Nested Parquet-on-FHIR | Existing queries and Spark tables read it. On hold until a site needs it | Large |
| Test against OpenMRS FHIR2 | The main community; it caps `_count` at 100 and has quirks the Java pipeline works around | Small |

### Useful, not blocking

| Feature | Why | Size |
|---|---|---|
| Parallel page fetching | Faster first loads on big servers. HAPI's `_getpagesoffset` as the Java pipeline does it, or several types at once | Medium |
| Spark or Hive table definitions | Sites with a Spark Thrift server could keep it: write a `CREATE TABLE … USING PARQUET` file next to `catalog.sql` | Small |
| Snapshots and retention | Lets a run be rolled back and queries read a consistent set. Could be timestamped directories with the catalog pointing at the latest | Medium |
| A schedule and status endpoint | For sites that want a long-running service instead of cron: a `serve` mode with a schedule, `/status`, `/run` and Prometheus metrics | Medium to large |
| `refresh` from files | Run the warehouse from NDJSON or Bundle files (exports, Synthea) without a server | Small |
| STU3 | Only if a site still runs STU3 | Medium |
| HAPI database source | Faster than search for very large HAPI servers, and sees deletions. Ties fhir-pipes to HAPI's schema | Medium |
| Google Cloud FHIR store sink | Only for GCP sites | Small to medium |

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
