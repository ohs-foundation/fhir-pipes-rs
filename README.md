# fhir-pipes

Data pipelines into and out of FHIR servers, in one binary with no runtime
dependencies.

- **Out of FHIR:** build a SQL-on-FHIR warehouse of Parquet tables from a FHIR
  server, or download resources as NDJSON.
- **Transforms:** turn FHIR resources into datasets for other tools. The first
  is `locations`, which turns the Location registry into GeoParquet for GIS
  tools.
- **Into FHIR:** load resources into a FHIR server with version-checked
  writes, including edits made in a GIS tool and data imported from DHIS2.

fhir-pipes is intended to move into
[fhir-data-pipes](https://github.com/ohs-foundation/fhir-data-pipes) as a
lighter replacement for its Java pipeline. It brings together two Recode Labs
tools, kiln and fhir-warehouse, and the octofhir/sof SQL-on-FHIR engine. The
reasoning and the plan are in
[docs/notes/2026-10-14-consolidation-with-fhir-data-pipes.md](docs/notes/2026-10-14-consolidation-with-fhir-data-pipes.md).

## Building a SQL-on-FHIR warehouse

`fhir-pipes refresh` downloads the resources a directory of ViewDefinitions
reads, runs the views, and writes each view as a Hive-partitioned Parquet
table, with a manifest of what it wrote.

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
map tiles, run your own commands afterwards, and push the warehouse to object
storage. [docs/refresh.md](docs/refresh.md) covers the steps, the config file,
partitioning and hooks.

### How the views run

The ViewDefinition engine is in [crates/sof](crates/sof), brought in from
[octofhir/sof](https://github.com/octofhir/sof). It evaluates views in memory
on every CPU, reading each resource type once however many views use it, and
uses the [octofhir-fhirpath](https://crates.io/crates/octofhir-fhirpath)
crate for FHIRPath. It passes the official SQL-on-FHIR v2.1 test suite (144
of 144 cases). [crates/sof-lint](crates/sof-lint) checks each view's
structure before anything runs, so a broken view stops the refresh with the
file and the rule it breaks.

The 12 views in fhir-data-pipes (`docker/config/views`) run unchanged.

## Commands

| Command | Direction | What it does | Docs |
|---|---|---|---|
| `refresh` | out | Build or refresh a SQL-on-FHIR Parquet warehouse | [refresh.md](docs/refresh.md) |
| `views run`, `sql`, `validate`, `lint`, `test` | | Run views over local FHIR files or PostgreSQL, generate their SQL, check them, run SQL-on-FHIR test cases | below |
| `export` | out | Download every resource of the given types as NDJSON | below |
| `extract` | out | Copy Locations and Organizations into a local snapshot, incrementally | [locations.md](docs/locations.md) |
| `transform locations` | transform | Snapshot → partitioned GeoParquet, the first transform | [locations.md](docs/locations.md) |
| `run` | out | `extract` then `transform locations` | [locations.md](docs/locations.md) |
| `inspect` | | Summarise a GeoParquet dataset | [locations.md](docs/locations.md) |
| `diff` | in | Edited GeoJSON or GeoParquet → changed FHIR resources | [locations.md](docs/locations.md) |
| `load` | in | Send NDJSON resources to a FHIR server in transaction bundles, with version checks | [locations.md](docs/locations.md#load) |
| `index` | in | Add spatial index identifiers (quadkey, geohash) to Locations | [locations.md](docs/locations.md#index) |
| `population` | in | Population Groups for admin units from a raster such as WorldPop | [locations.md](docs/locations.md#population) |
| `dhis2 locations`, `measures`, `reports` | in | DHIS2 org units, indicators and data → FHIR NDJSON | [dhis2.md](docs/dhis2.md) |

Run `fhir-pipes <command> --help` for every option. NDJSON paths ending in
`.gz` are read and written compressed.

`views` works with ViewDefinitions directly, without a warehouse:

```sh
fhir-pipes views run patient_flat.json --input raw/Patient.ndjson          # CSV to stdout
fhir-pipes views run views/ --input raw/ --output parquet --out tables/   # one file per view
fhir-pipes views sql patient_flat.json --dialect duckdb                     # the SQL for a view
fhir-pipes views validate patient_flat.json                                 # structure checks
fhir-pipes views lint patient_flat.json --package hl7.fhir.r4.core          # selectors against FHIR R4
fhir-pipes views test crates/sof/tests/spec                                 # the official test cases
```

`views run --db URL` runs against PostgreSQL holding FHIR resources as JSONB
instead of files. A directory of views is read as `refresh` reads it.

`export` downloads every resource of each `--type` to `DIR/<TYPE>.ndjson`:

```sh
fhir-pipes export --server https://fhir.example.org/fhir --type Patient --type Observation --out raw/
```

It pages the search, writes each file beside the target and renames it into
place, so a failed run leaves the previous download intact. Each run is a full
download. When the last page is full and has no next link, the server's count
is checked and a difference is reported as `count_mismatch`.

## Connecting to a FHIR server

Any FHIR R4 server that supports standard search with `_lastUpdated`,
`Bundle.link` paging and transaction bundles works. fhir-pipes does not depend
on any vendor's extensions.

A server that needs authentication gets one of two kinds:

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

The two are mutually exclusive; with neither, requests carry no
Authorization header.

```sh
export FHIR_PIPES_CLIENT_SECRET=...          # keep the secret out of shell history
fhir-pipes export --server https://gateway.example.org/fhir \
  --client-id key-me --token-url https://auth.example.org/realms/R/protocol/openid-connect/token \
  --type Patient --out raw/
```

`--timeout` is the total deadline for one HTTP request, 300 seconds by
default. Connection errors, 429 and 5xx responses are retried up to
`--retries` times with exponential backoff and jitter, honouring a numeric
`Retry-After`. Any other 4xx is final.

## Not done yet

- `refresh` downloads every resource the views read on every run. Only the
  Location registry is extracted incrementally so far.
- There is no nested Parquet-on-FHIR output (fhir-data-pipes' SQL-on-FHIR v1
  tables) yet.
- The PostgreSQL and async dependencies that `views run --db` and
  `views sql` need are always compiled in; they are not yet optional.
- `bake` and `bake-points`, which turn GRID3 and CSV files into Locations,
  are still in the Python package under `python/` (named `kiln`); see
  [python/README.md](python/README.md).

## Building and testing

Requires a stable Rust toolchain.

```sh
cargo build --release
./target/release/fhir-pipes --help
cargo test --workspace
```

Release binaries are built in CI for macOS (arm64, x86_64), Linux (x86_64 and
aarch64, statically linked against musl) and Windows (x86_64), as single files
with no runtime dependencies.

Some tests check output with DuckDB when it is on `PATH` and skip that check
otherwise. DuckDB is never linked into fhir-pipes. The SQL-on-FHIR
conformance suite runs in memory as part of `cargo test`; the generated-SQL
runs need DuckDB, or PostgreSQL through `SOF_CONFORMANCE_DB`.

## Repository layout

```
Cargo.toml        the fhir-pipes binary and the workspace
src/
  main.rs, cli.rs, error.rs
  warehouse/      refresh: config, views, partitioning, manifest, tiles, hooks, push
  views/          views: run, sql, validate, lint, test
  export.rs       export
  extract/        FHIR client with retry and OAuth, paging, boundary fetch and cache
  snapshot/       state.json, merge by id
  load/           parents-first ordering, transaction bundles with ifMatch
  fhir/           resource parsing, NDJSON reading and writing
  transform.rs, index/, geometry/, write/, inspect.rs, run.rs
                  the locations transform: hierarchy, geometry, GeoParquet
  diff/           edits back to FHIR resources
  cells.rs        index: spatial index identifiers
  raster/, population.rs
                  population Groups from a raster
  dhis2/          DHIS2 import
crates/
  sof/            fhir-pipes-sof: the ViewDefinition engine (from octofhir/sof)
  sof-lint/       fhir-pipes-sof-lint: ViewDefinition checks
tests/            integration tests per command; fixtures under tests/fixtures
python/           bake and bake-points (still the Python kiln package)
docs/
  refresh.md, locations.md, dhis2.md, qgis.md
  notes/          the consolidation notes
  superpowers/    kiln's design specs, plans and spikes
```

## License

Apache-2.0. `crates/sof` and `crates/sof-lint` are MIT OR Apache-2.0, from
octofhir/sof by the OctoFHIR Team.
