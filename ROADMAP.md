# Roadmap

What fhir-pipes does now, what is next, and what was decided against. Update
it with each release. The reasoning behind the plan is in
[docs/notes/2026-10-14-consolidation-with-fhir-data-pipes.md](docs/notes/2026-10-14-consolidation-with-fhir-data-pipes.md).

Current release: **v0.4.0** (2026-10-15).

## What fhir-pipes does now

"Since" is the first tagged release that has it. v0.3.0 was the first tagged
release of fhir-pipes; earlier work was in kiln and fhir-warehouse.

### Out of FHIR

| Capability | Command | Since |
|---|---|---|
| Build a SQL-on-FHIR warehouse: download what the ViewDefinitions read, run them, write Hive-partitioned Parquet and a manifest | `refresh` | 0.3.0 |
| Download only what changed (`_lastUpdated` watermark per type, merged by id); a type whose count differs from the server's is downloaded in full, which picks up deletions | `refresh`, `export --incremental` | 0.3.0 |
| Download every resource of the given types as NDJSON | `export` | 0.3.0 |
| Download through a system-level Bulk Data `$export` instead of search: `_since` the last export's `transactionTime`, merged by id, `deleted` resources removed, an interrupted export resumed from its saved status URL, cancelled after `--bulk-timeout`. Group and Patient exports are not supported yet | `export --bulk`, `refresh` (`fetch = "bulk"`) | next release |
| Copy Locations and Organizations into a local snapshot, incrementally | `extract` | 0.3.0 |
| A refresh is all-or-nothing: tables, `manifest.json` and `catalog.sql` are staged and swapped in together at the end, so a run that fails leaves the previous run's warehouse as it was (#35) | `refresh` | next release |
| Map tiles of the Location registry (PMTiles, through duckdb and tippecanoe) | `refresh` (tiles step) | 0.3.0 |
| Run your own commands after a refresh; push the warehouse to object storage with rclone | `refresh` (hooks, `--push`) | 0.3.0 |
| Load the view tables into PostgreSQL, all in one transaction, over TLS when the connection string asks (#25) | `refresh` (`[database]`) | next release |
| A DuckDB catalog: `catalog.sql` with a view per table, read from a bucket URL, plus your own views; optionally `catalog.duckdb` to attach by URL (needs the duckdb command) | `refresh` (`[catalog]`) | next release |
| Authenticate to a FHIR server with a bearer token or an OAuth client (client-credentials grant, refreshed before expiry); retry 429, 5xx and connection errors. The `dhis2` commands use a DHIS2 personal access token or basic auth instead | `extract`, `export`, `run`, `load`, `refresh` | 0.3.0 |
| HTTP Basic auth to a FHIR server (#26) | `extract`, `export`, `run`, `load`, `refresh` | next release |

### ViewDefinitions

| Capability | Command | Since |
|---|---|---|
| Run views over NDJSON files, or over FHIR resources stored as JSONB in PostgreSQL (`postgres` feature) | `views run` | 0.3.0 |
| Read Bundle files directly: `urn:uuid` references between entries resolve to `Type/id`, and input directories are read with their subdirectories | `views run --input` | next release |
| Generate the SQL for a view (PostgreSQL or DuckDB), or a `CREATE TABLE` for its columns | `views sql` | 0.3.0 |
| Check a view's structure, and lint it for the portable FHIRPath subset; check selectors against a FHIR package (`fhir-packages` feature) | `views validate`, `views lint` | 0.3.0 |
| Run the official SQL-on-FHIR v2.1 test cases (144 of 144 pass) | `views test` | 0.3.0 |

### Transforms

| Capability | Command | Since |
|---|---|---|
| Location registry snapshot → partitioned GeoParquet with the admin hierarchy, for GIS tools | `transform locations`, `run` | 0.3.0 |
| Summarise a GeoParquet dataset | `inspect` | 0.3.0 |

### Into FHIR

| Capability | Command | Since |
|---|---|---|
| Edited GeoJSON or GeoParquet → the changed FHIR resources | `diff` | 0.3.0 |
| Send NDJSON to a FHIR server in transaction bundles, with version checks | `load` | 0.3.0 |
| Spatial index identifiers (quadkey, geohash) on Locations | `index` | 0.3.0 |
| Population Groups for admin units or catchments from a raster such as WorldPop | `population` | 0.3.0 |
| DHIS2 org units → Locations, indicators → Measures, values → MeasureReports | `dhis2 locations`, `measures`, `reports` | 0.3.0 |
| MeasureReports record their source as a CodeableConcept (`dhis2-analytics`, `dhis2-data-values`) | `dhis2 reports` | 0.4.0 |

### Profiles

| Capability | Command | Since |
|---|---|---|
| An IG's extra columns, admin units, codes and URLs from a TOML profile, given as a file or an https URL | `--profile` | 0.3.0 |
| `extension_code` columns that read and write a CodeableConcept under a given `system` | `--profile` | 0.3.1 |
| Compare a profile with its IG: extensions, contexts, value types, bindings, code systems, profiles, identifier slices | `profile check` | 0.4.0 |

### Builds

| Capability | Since |
|---|---|
| Release binaries for macOS (arm64, x86_64), Linux (x86_64, aarch64, static) and Windows (x86_64), with `SHA256SUMS` | 0.3.0 |
| CI on Linux, macOS and Windows for every PR | 0.3.0 |

## Next

| Item | Status | Notes |
|---|---|---|
| Profile builder: write a starting profile from an IG | Planned | A `profile build` subcommand, reusing the IG loader in `src/profile/check.rs` |
| Port `bake` and `bake-points` (GRID3 and CSV → Locations) from Python | Planned | Still in `python/` |
| Move the FHIR client, paging, snapshot merge and loader into their own crate | Planned | Step 1 of the consolidation plan |
| Put geo, rasters and DHIS2 behind Cargo features | Planned | So an analytics-only build stays small; only `postgres` (on by default) and `fhir-packages` are optional today |
| Nested Parquet-on-FHIR (SQL-on-FHIR v1) output | Waiting for a user | Built only when a user needs it, such as a site moving from the Java pipeline whose queries read its nested tables. ICR does not depend on nested tables (it reads view tables and the `locations` GeoParquet), and the raw NDJSON stays queryable with DuckDB's `read_json`. If built, the schema comes from StructureDefinitions to match Bunsen's, with a full-resource JSON column as the fallback |
| Bulk Data `Group/[id]/$export` and `Patient/$export` | Not started | `export --bulk` is system level only |
| Fetch resources a patient compartment references (Organization, Practitioner, Location) | Not started | The Java pipeline does this |
| Versioned run directories: each refresh in its own directory, with the manifest and catalog naming the current one, and old runs kept for rollback | Not started | Option 2 of #35, like the Java pipeline's timestamped snapshots. Changes the paths readers use, so it needs a migration. A refresh is already all-or-nothing without it |
| Stage the tiles and the `views/` copies with the tables | Not started | A failed refresh can still leave new tiles and view copies beside the previous run's tables |

## Moving into fhir-data-pipes

| Step | Status |
|---|---|
| Bring kiln, fhir-warehouse and sof together in one repo and binary | Done (2026-10-14) |
| `refresh` downloads only what changed for every view's resource type | Done (0.3.0) |
| Move ICR-specific mappings out of the code into a profile | Done (0.3.0) |
| Reply on the fhir-data-pipes Rust RFC (`exp/rust-fhir-pipes`) proposing this tool | Open |
| Agree the two-way scope (into FHIR as well as out) with the fhir-data-pipes maintainers | Open |
| Sign the fhir-data-pipes CLA for Recode Labs | Open |
| Move the code into fhir-data-pipes as a Rust workspace | Not started |
| Archive kiln and fhir-warehouse as separate repos | Done |

## Compared with the fhir-data-pipes Rust RFC

The RFC is `experimental/fhir-pipes-rs/ARCHITECTURE_RFC.md` and `README.md`
on the `exp/rust-fhir-pipes` branch of fhir-data-pipes. It proposes replacing
the Java pipeline (Beam, Flink, HAPI, Bunsen, Avro, Spark Thrift) with one
Rust binary. This table puts each part of its proposed design, and each item
on its own outstanding list, next to fhir-pipes.

### Proposed design

| RFC | fhir-pipes |
|---|---|
| One native binary on arrow-rs and parquet, CLI first, run from cron or CI instead of a Spring Boot and Flink controller | Yes. `refresh` takes the controller's role |
| Evaluate ViewDefinitions in Rust rather than compiling them to SQL | Yes, in memory (`crates/sof`); SQL generation is also available (`views sql`) |
| Read `$export` NDJSON, FHIR search Bundles, and local Bundles | NDJSON, Bundles, JSON resources and directories of them, subdirectories included (`views run --input`); FHIR search and system-level `$export` (`export`, `export --bulk`, `refresh`; `$export` in the next release) |
| Resolve `urn:uuid` references inside transaction and collection Bundles | Yes, for `views run --input` (next release) |
| Incremental merge by id and `meta.lastUpdated`, dropping deleted resources | Yes, per type, with a `_lastUpdated` watermark; deletions are found when the server's count differs, then the type is downloaded in full. With `$export`, the manifest's `deleted` resources are removed without a full download (next release) |
| Nested Parquet-on-FHIR tables (SQL-on-FHIR v1) | No; see [Next](#next) |
| Flat view tables in Parquet | Yes, Hive-partitioned, with a manifest |
| `schema.sql` (`CREATE TABLE` per view) | `views sql --ddl` (ANSI, PostgreSQL or DuckDB types) |
| `init_duckdb.sql` (a DuckDB view per table) | `catalog.sql`, with paths for a bucket URL, and an optional `catalog.duckdb` to attach by URL (next release) |
| Local SQL studio and dashboards | No |
| On-device use on Android and iOS (`MOBILE_KMP_RFC.md`) | No |

### The RFC's outstanding list

| # | RFC outstanding item | fhir-pipes |
|---|---|---|
| 1 | Network sources: paged FHIR search, `_lastUpdated` incremental sync, OAuth2 and bearer tokens, retry with backoff | Done, including a system-level Bulk Data `$export` client (next release) |
| 2 | Direct relational database sinks (PostgreSQL) | `refresh` with `[database]` loads the view tables into PostgreSQL, with TLS (next release) |
| 3 | Schema from StructureDefinitions, including IG profiles | Not needed for the flat tables, which take their columns from the views. Would be the basis of nested output. `profile check` already reads an IG's StructureDefinitions |
| 4 | Complete FHIRPath, passing the official SQL-on-FHIR v2 tests | Done for views evaluated in memory (`views run --input`, `refresh`): `octofhir-fhirpath`, 144 of 144. Generated SQL (`views sql`, and `views run --db`, which uses it) supports fewer functions; `substring()`, for one, is refused |
| 5 | Partitioned merging for large warehouses | Partly. Downloads are incremental and output is partitioned, but each refresh rewrites every view table from the whole download |
| 6 | Fetch resources a patient compartment references | Not done |
| 7 | Object storage (S3, GCS, Azure) | Push only, through rclone. No reading from object storage |

### Its community questions

| RFC question | fhir-pipes answer |
|---|---|
| A single CLI run by cron, Airflow or CI instead of the Spring Boot and Flink controller? | Yes. `refresh` is one command with a TOML config, meant to be run that way |
| Query Parquet with DuckDB or PostgreSQL instead of Spark Thrift and Hive? | Yes: Parquet with DuckDB, and the view tables in PostgreSQL (item 2) |

## Decided against for now

| Item | Why |
|---|---|
| Replace duckdb and tippecanoe in the tiles step with pure Rust | They work; the tiles step stays optional |
| Switch `crates/sof` and `crates/sof-lint` to the upstream octofhir-sof crates | Upstream has no optional `packages` feature, so every build would pull in its package downloader (reqwest with aws-lc) again. Upstream fixes are ported in instead (as in #14) |
