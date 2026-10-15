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
| Copy Locations and Organizations into a local snapshot, incrementally | `extract` | 0.3.0 |
| Map tiles of the Location registry (PMTiles, through duckdb and tippecanoe) | `refresh` (tiles step) | 0.3.0 |
| Run your own commands after a refresh; push the warehouse to object storage with rclone | `refresh` (hooks, `--push`) | 0.3.0 |
| Authenticate with a bearer token or an OAuth client (client-credentials grant, refreshed before expiry); retry 429, 5xx and connection errors | every command that talks to a server | 0.3.0 |

### ViewDefinitions

| Capability | Command | Since |
|---|---|---|
| Run views over NDJSON files, or over FHIR resources stored as JSONB in PostgreSQL (`postgres` feature) | `views run` | 0.3.0 |
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
| Put geo, rasters and DHIS2 behind Cargo features | Planned | So an analytics-only build stays small; only `postgres` and `fhir-packages` are optional today |
| Nested Parquet-on-FHIR (SQL-on-FHIR v1) output | If needed | Only for compatibility with existing fhir-data-pipes warehouses. Build it on a schema from StructureDefinitions, with a full-resource JSON column as the fallback |
| FHIR Bulk Data `$export` client | Not started | `export` uses search paging |
| Write view tables straight into PostgreSQL | Not started | `views sql` generates the SQL; there is no loader |
| Fetch resources a patient compartment references (Organization, Practitioner, Location) | Not started | The Java pipeline does this |
| Resolve `urn:uuid` references when reading transaction and collection Bundles | Not started | The Java pipeline and the RFC prototype do this |

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
| Read `$export` NDJSON, FHIR search Bundles, and local Bundles | NDJSON, Bundles, JSON resources and directories of them (`views run --input`); FHIR search (`export`, `refresh`). No `$export` client |
| Resolve `urn:uuid` references inside transaction and collection Bundles | No |
| Incremental merge by id and `meta.lastUpdated`, dropping deleted resources | Yes, per type, with a `_lastUpdated` watermark; deletions are found when the server's count differs, then the type is downloaded in full |
| Nested Parquet-on-FHIR tables (SQL-on-FHIR v1) | No; see [Next](#next) |
| Flat view tables in Parquet | Yes, Hive-partitioned, with a manifest |
| `schema.sql` (`CREATE TABLE` per view) | `views sql --ddl` (ANSI, PostgreSQL or DuckDB types) |
| `init_duckdb.sql` (a DuckDB view per table) | No; the manifest lists the tables |
| Local SQL studio and dashboards | No |
| On-device use on Android and iOS (`MOBILE_KMP_RFC.md`) | No |

### The RFC's outstanding list

| # | RFC outstanding item | fhir-pipes |
|---|---|---|
| 1 | Network sources: paged FHIR search, `_lastUpdated` incremental sync, OAuth2 and bearer tokens, retry with backoff | Done, except a Bulk Data `$export` client |
| 2 | Direct relational database sinks (PostgreSQL) | Not done. `views sql` generates the SQL, and `views run --db` reads FHIR from PostgreSQL, but nothing writes view rows into a database |
| 3 | Schema from StructureDefinitions, including IG profiles | Not needed for the flat tables, which take their columns from the views. Would be the basis of nested output. `profile check` already reads an IG's StructureDefinitions |
| 4 | Complete FHIRPath, passing the official SQL-on-FHIR v2 tests | Done: `octofhir-fhirpath`, 144 of 144 |
| 5 | Partitioned merging for large warehouses | Partly. Downloads are incremental and output is partitioned, but each refresh rewrites every view table from the whole download |
| 6 | Fetch resources a patient compartment references | Not done |
| 7 | Object storage (S3, GCS, Azure) | Push only, through rclone. No reading from object storage |

### Its community questions

| RFC question | fhir-pipes answer |
|---|---|
| A single CLI run by cron, Airflow or CI instead of the Spring Boot and Flink controller? | Yes. `refresh` is one command with a TOML config, meant to be run that way |
| Query Parquet with DuckDB or PostgreSQL instead of Spark Thrift and Hive? | Yes, for Parquet. PostgreSQL output is item 2 above |

## Decided against for now

| Item | Why |
|---|---|
| Replace duckdb and tippecanoe in the tiles step with pure Rust | They work; the tiles step stays optional |
| Switch `crates/sof` and `crates/sof-lint` to the upstream octofhir-sof crates | Upstream has no optional `postgres` or `packages` features, so the default build would pull in sqlx and reqwest again. Upstream fixes are ported in instead (as in #14) |
