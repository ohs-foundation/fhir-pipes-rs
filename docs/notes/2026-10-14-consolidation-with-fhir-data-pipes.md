# Consolidating kiln, fhir-warehouse and fhir-pipes-rs into fhir-data-pipes

Notes from a review on 2026-10-14. The aim is one tool, not several in
parallel.

## The projects involved

| Project | Where | What it is | State on 2026-10-14 |
|---|---|---|---|
| fhir-data-pipes | `ohs-foundation/fhir-data-pipes` | Java pipeline (Beam, Flink, HAPI, Bunsen, Avro) that turns FHIR into Parquet-on-FHIR and SQL-on-FHIR views, with a Spring Boot controller that runs it on a schedule | Production, Apache-2.0, CLA required |
| fhir-pipes-rs | `experimental/fhir-pipes-rs` on branch `exp/rust-fhir-pipes` of fhir-data-pipes, commit `37eb7f6` | Rust proof of concept and RFC proposing a replacement for the Java stack | One commit, about 1,770 lines, 3 unit tests |
| kiln | `recodelabs/kiln` | FHIR Location registry to GeoParquet and back; DHIS2, population and spatial index imports | 242 commits since 2026-07-27, about 23k lines plus 6.7k lines of tests (459 tests), CI, release binaries |
| fhir-warehouse | `recodelabs/fhir-warehouse` | Refresh job: calls kiln, runs ViewDefinitions with sof, writes partitioned Parquet, tiles, manifest, hooks, push | 2 commits, about 2,300 lines, 22 tests, MIT |
| sof | `recodelabs/sof`, fork of `octofhir/sof` | SQL-on-FHIR v2 engine: in-memory evaluator and PostgreSQL generator | 4 commits ahead of upstream (native temporal Parquet types, parallel evaluation), MIT OR Apache-2.0 |

## How the pieces map onto each other

kiln does the work of moving data. fhir-warehouse decides what runs and in
what order for a deployment. fhir-data-pipes already has both layers in one
repo.

| Layer | fhir-data-pipes (Java) | Recode Labs equivalent | fhir-pipes-rs |
|---|---|---|---|
| Fetch from FHIR (search, bulk export, HAPI database) | `pipelines/batch` | kiln `extract`, `export` | Local files only |
| Incremental merge | `ParquetMerger` (full rewrite of the warehouse each run) | kiln snapshot and watermark (Location and Organization only) | In-memory merge of base and delta files |
| Run ViewDefinitions, write Parquet | `pipelines/batch`, `common/view` | sof, called by fhir-warehouse | Own evaluator |
| Nested Parquet-on-FHIR (SQL-on-FHIR v1) | Bunsen and Avro | none | Yes, with inferred schema |
| Scheduled runs, warehouse snapshots, database or Hive output | `pipelines/controller` (`incrementalSchedule`, `dwhRootPrefix`, `viewDefinitionsDir`, `sinkDbConfigPath`) | fhir-warehouse `refresh` and its TOML | none |
| Geo projection, GIS round trip, DHIS2 and population import | none | kiln | none |

## Findings

### fhir-pipes-rs

Built and run against the repo's `synthea-hiv/sample_data_segments` (108 MB,
46,497 resources) and the 12 views in `docker/config/views`: 1.12 s wall
time, 2.2 GB peak memory. The RFC claims it runs in 256 MB.

Problems found in the code:

1. Not lossless, despite the claim. `extension`, `modifierExtension` and
   `contained` are dropped at every level except a few promoted top-level
   extensions.
2. The nested schema is inferred from each run's data, so columns change
   when the data does. All numbers become f64.
3. References get both `patientId` and `PatientId`. Case-insensitive engines
   (DuckDB, Postgres, Spark) see duplicate columns; DuckDB renames one to
   `patientid_1`.
4. FHIRPath is handled by string splitting. Only `=`, `!=`, `where()`,
   `first()`, `ofType()`, `getReferenceKey()` and `getResourceKey()` work.
   The README says `and`/`or`/`not` are supported; they are not, and an
   expression using them gives wrong results without an error.
5. The choice-type prefix list has `" Serviced"` (leading space, capital S),
   so `serviced[x]` is never split.
6. `meta.lastUpdated` is compared as a string when merging.
7. Everything is held in memory, plus one full clone for grouping. Each type
   is written as a single row group.

The RFC's analysis of the Java pipeline (four object conversions per
resource, Beam and Flink workarounds, full warehouse rewrite on every
incremental run) is useful and worth keeping.

### sof compared with the fhir-pipes-rs view engine

Same 46,497 resources (Bundles flattened to NDJSON with `urn:uuid`
references resolved, as the PoC does) and the same 12 views. Outputs
compared row by row in DuckDB.

| | fhir-pipes-rs | sof |
|---|---|---|
| Views with identical output | 11 of 12 | 11 of 12 |
| `location_flat` | 175 rows differ: decimals pass through f64 (`41.735496999999995` for `41.735497`) | Keeps the exact decimal text |
| Wall time | 1.12 s, including nested Parquet output | 0.79 s, views only |
| Peak memory | 2.2 GB | 0.87 GB |
| FHIRPath | String splitting | Full parser (`octofhir-fhirpath`) |
| Official SQL-on-FHIR v2.1 tests | Not run | 144/144 per its `CONFORMANCE.md` (not re-run here) |

The only thing fhir-pipes-rs has that the Recode Labs set does not is
nested Parquet-on-FHIR output, which matters only for compatibility with
existing fhir-data-pipes warehouses.

## Decisions

1. **One tool, in fhir-data-pipes.** A data pipeline can go both ways, so
   the project covers reading from FHIR and writing into it.
2. **kiln's code is the engine.** The FHIR client and snapshot become a
   shared crate. Geo projection, `diff`/`load`, DHIS2, population and index
   become modules behind Cargo features.
3. **Geo and import stay in the same tool**, not a separate one. `transform`
   and `diff` share a column mapping and must change together.
4. **fhir-warehouse becomes the `refresh` command** of that tool. It plays
   the role the Java controller plays today. kiln's commands remain
   available as individual subcommands.
5. **sof is an external dependency**, not vendored. The fork's changes go
   upstream to `octofhir/sof`.
6. **Drop the fhir-pipes-rs code; keep its RFC** and use its 12 views as an
   acceptance test.
7. **Deployment configuration lives in its own repo.** ICR-specific parts
   (`icr_` table prefix, country partition lookup, tile layers, profile
   extension columns, DHIS2 mapping) move out of code into an ICR config repo
   of ViewDefinitions, TOML and hooks.

## Rules for the combined tool

- Writes to a FHIR server are separate subcommands that need write
  credentials and are never part of a default `refresh`. Keep the NDJSON
  checkpoint and `--dry-run` before every load.
- Geo, rasters and DHIS2 sit behind Cargo features so an analytics-only
  build stays small.
- tiles (duckdb, tippecanoe) and push (rclone) stay optional steps; the core
  runs as a single binary.
- Column mappings used by the round trip are simple path-to-column entries
  that can be reversed. Hierarchy and geometry stay built in, since they
  come from standard FHIR (`partOf`, `location-boundary-geojson`).
- The snapshot layout is documented as a format, since `refresh`, `views`
  and `transform` all read it.

## Gaps to close

1. `views` is not incremental: `kiln export` downloads every resource of each
   type on every run. Generalise the snapshot from Location and Organization
   to any resource type.
2. fhir-warehouse calls kiln as a separate program. Link them as libraries
   in one binary.
3. fhir-warehouse depends on the sof fork's git `main`. Upstream the fork's
   changes and depend on a released crate.
4. Nested Parquet-on-FHIR, if existing fhir-data-pipes users need it. Build
   it on an explicit schema from StructureDefinitions, with a full-resource
   JSON column as the fallback.
5. Port the Python `bake` and `bake-points` as sources.

## Order of work

1. Inside kiln, move the FHIR client, paging, snapshot merge and loader into
   a separate crate in a workspace, keeping the existing tests passing.
2. Generalise the snapshot to any resource type; move ICR mappings into a
   profile file.
3. Send the sof fork's changes upstream.
4. Reply on the fhir-pipes-rs RFC proposing this set. It covers the RFC's
   outstanding item 1 (network sources), item 4 (full FHIRPath, via sof),
   item 7 (object storage, via push) and Postgres output (via sof's SQL
   generation). Item 5 (incremental merge) is covered for Locations only
   until step 2 is done.
5. Agree the two-way scope with the fhir-data-pipes maintainers. The project
   currently describes itself as FHIR to analytics only.
6. Move the code into a Rust workspace in fhir-data-pipes, with
   fhir-warehouse as `refresh`.
7. Retire kiln and fhir-warehouse as separate repos.

## Licensing

- fhir-data-pipes: Apache-2.0, CLA required (`CONTRIBUTING.md`). Someone at
  Recode Labs needs to sign it.
- kiln: Apache-2.0 in `Cargo.toml`, no LICENSE file. Add one.
- fhir-warehouse: MIT, copyright Recode Labs. Relicense to Apache-2.0 when
  contributing.
- sof: MIT OR Apache-2.0, usable as a dependency as is.

## Reproducing the comparison

```sh
# fhir-pipes-rs
git -C fhir-data-pipes archive origin/exp/rust-fhir-pipes experimental/fhir-pipes-rs | tar -x -C work
cd work/experimental/fhir-pipes-rs && cargo build --release
/usr/bin/time -l ./target/release/fhir-pipes-rs etl \
  --input fhir-data-pipes/synthea-hiv/sample_data_segments \
  --views-dir fhir-data-pipes/docker/config/views --output out

# sof: flatten Bundles to NDJSON, resolving urn:uuid references
find fhir-data-pipes/synthea-hiv/sample_data_segments -name '*.json' | sort | while read f; do
  jq -c '(reduce .entry[] as $e ({}; .[$e.fullUrl] = ($e.resource.resourceType + "/" + $e.resource.id))) as $m
    | .entry[].resource
    | walk(if type=="object" and (.reference|type)=="string" and ($m[.reference])
           then .reference = $m[.reference] else . end)' "$f"
done > all.ndjson
/usr/bin/time -l octofhir-sof run views/ --input all.ndjson --output parquet --out sof-out

# compare one view in DuckDB (rows in a but not in b, and the reverse)
duckdb -c "select count(*) from (select columns(*)::varchar from 'out/materialized_views/location_flat.parquet'
                                 except all
                                 select columns(*)::varchar from 'sof-out/location_flat.parquet')"
```
