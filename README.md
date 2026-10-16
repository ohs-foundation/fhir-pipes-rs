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
[releases](https://github.com/ohs-foundation/fhir-pipes-rs/releases): macOS (arm64,
x86_64), Linux (x86_64, aarch64; static, no runtime dependencies) or Windows (x86_64).
Each release has a `SHA256SUMS` file to check it against.

```sh
# macOS on Apple Silicon, for example
curl -LO https://github.com/ohs-foundation/fhir-pipes-rs/releases/latest/download/fhir-pipes-aarch64-apple-darwin
chmod +x fhir-pipes-aarch64-apple-darwin && mv fhir-pipes-aarch64-apple-darwin /usr/local/bin/fhir-pipes
```

Or build it with Rust:

```sh
cargo install --git https://github.com/ohs-foundation/fhir-pipes-rs --locked fhir-pipes
```

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
map tiles, write a DuckDB catalog (a view per table, which a dashboard can
attach by URL) and Spark SQL tables for a Spark Thrift server, load the view tables into PostgreSQL, run your own commands afterwards, and push the warehouse to object
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

The 12 views in fhir-data-pipes (`docker/config/views`) run unchanged, and
give the same rows on its Synthea sample Bundles
(`synthea-hiv/sample_data_segments`) read directly as on the same resources
flattened to NDJSON.

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
| `views run`, `sql`, `validate`, `lint`, `test` | | Run views over local FHIR files or PostgreSQL, generate their SQL, check them, run SQL-on-FHIR test cases | below |
| `export` | out | Download every resource of the given types as NDJSON | below |
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
| `profile check` | | Compare a profile with its IG's extensions, code systems and profiles | [profiles.md](docs/profiles.md#checking-a-profile-against-its-ig) |

Run `fhir-pipes <command> --help` for every option. NDJSON paths ending in
`.gz` are read and written compressed.

`views` works with ViewDefinitions directly, without a warehouse:

```sh
fhir-pipes views run patient_flat.json --input raw/Patient.ndjson          # CSV to stdout
fhir-pipes views run views/ --input raw/ --output parquet --out tables/   # one file per view
fhir-pipes views sql patient_flat.json --dialect duckdb                     # the SQL for a view
fhir-pipes views validate patient_flat.json                                 # structure checks
fhir-pipes views lint patient_flat.json --shareable                         # the portable FHIRPath subset
fhir-pipes views test crates/sof/tests/spec                                 # the official test cases
```

A directory of views is read as `refresh` reads it. An `--input` directory is
read with its subdirectories, skipping hidden files. In a Bundle, a reference
to another entry by its `urn:uuid` fullUrl, as transaction Bundles from
Synthea have, is rewritten to that entry's `Type/id`, as a FHIR server would
store it, so `getReferenceKey()` and joins work on Bundle files. `views run --db URL` runs
against PostgreSQL holding FHIR resources as JSONB instead of files, and
`views lint --package hl7.fhir.r4.core` checks selectors against a FHIR
package it downloads; the second needs a build with the `fhir-packages`
feature (see [Building and testing](#building-and-testing)).

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
- The status URL is polled with the server's `Retry-After`, or a backoff from
  one second up to a minute without one. 429 is waited out the same way.
  Each new `X-Progress` is printed.
- The status URL is saved in `.export-state.json` as soon as the server
  returns it. A run that stops before the export is done (a crash, a lost
  connection) polls the same export next time instead of starting another.
  A saved export for other types or another `_since` is cancelled.
- `--bulk-timeout` (four hours by default) is how long a run waits. After
  that it cancels the export with a `DELETE` of the status URL and fails. It
  is also the time allowed for each file's download.
- The output files are downloaded with the bearer token only when the
  manifest says `requiresAccessToken`.
- Resources in the manifest's `deleted` Bundles are removed from the files,
  unless the same export also returned them. The server's count is still
  checked after a merge, as for search, because not every server lists
  deletions (HAPI does not); a type whose count differs is exported again in
  full.
- Every type's watermark becomes the manifest's `transactionTime`.
- An `OperationOutcome` from the kick-off or the status URL, or an error or
  fatal issue in the manifest's `error` files, fails the run with its text.
  Warnings and information go in the report.
- After the files are read, the status URL gets a `DELETE` so the server can
  remove them.

Only system-level export is supported. `Group/[id]/$export` and
`Patient/$export` are not supported yet. Files are downloaded one after
another. HAPI asks clients to wait two minutes between polls, so even a small
export from HAPI takes about two minutes.

## Connecting to a FHIR server

Any FHIR R4 server that supports standard search with `_lastUpdated`,
`Bundle.link` paging and transaction bundles works. fhir-pipes does not depend
on any vendor's extensions. `export --bulk` also needs the server's
system-level `$export` (in HAPI, `hapi.fhir.bulk_export_enabled=true`).

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
`--retries` times with exponential backoff and jitter, honouring a numeric
`Retry-After`. Any other 4xx is final.

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

## Not done yet

The open items are in [ROADMAP.md](ROADMAP.md#next). The main ones:

- There is no nested Parquet-on-FHIR output (fhir-data-pipes' SQL-on-FHIR v1
  tables) yet.

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

Release binaries are built in CI for macOS (arm64, x86_64), Linux (x86_64 and
aarch64, statically linked against musl) and Windows (x86_64), as single files
with no runtime dependencies. CI tests every PR on Linux, macOS and Windows; the
release builds are slow, so they run for a version tag (which publishes them),
when run by hand from the Actions tab, for a change to `Cargo.toml`,
`Cargo.lock` or the workflows, and weekly.

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
  export/         export: search paging, and Bulk Data $export (bulk.rs)
  extract/        FHIR client with retry and OAuth, paging, boundary fetch and cache
  snapshot/       state.json, merge by id
  load/           parents-first ordering, transaction bundles with ifMatch
  sync/           sync: copy an export directory to a second FHIR server
  fhir/           resource parsing, NDJSON reading and writing
  transform.rs, index/, geometry/, write/, inspect.rs, run.rs
                  the locations transform: hierarchy, geometry, GeoParquet
  diff/           edits back to FHIR resources
  cells.rs        index: spatial index identifiers
  raster/, population.rs
                  population Groups from a raster
  dhis2/          DHIS2 import
  import/         import location-boundaries and location-sites: GeoJSON and CSV to Locations
  profile/        profiles: loading, and `profile check` against an IG
crates/
  sof/            fhir-pipes-sof: the ViewDefinition engine (from octofhir/sof)
  sof-lint/       fhir-pipes-sof-lint: ViewDefinition checks
tests/            integration tests per command; fixtures under tests/fixtures
docs/
  refresh.md, locations.md, sync.md, dhis2.md, qgis.md, profiles.md, icr-profile.md
  notes/          the consolidation notes
  superpowers/    kiln's design specs, plans and spikes
```

## License

Apache-2.0. `crates/sof` and `crates/sof-lint` are MIT OR Apache-2.0, from
octofhir/sof by the OctoFHIR Team.
