### NOTE: [OHS-Labs Repository]: __This is an experimental library. Not currently supported. For the supported version go to the fhir-data-pipes repository__

# fhir-pipes

fhir-pipes is a high performance rust library/service for pulling data out of and into FHIR servers.  fhir-pipes is available as a binary in linux, windows, osx with no external runtime dependencies. It builds a SQL-on-FHIR warehouse of Parquet tables from a FHIR
server, downloads resources as NDJSON, turns the Location registry into
GeoParquet, and loads resources back with version-checked writes. It also
turns records from other systems, such as DHIS2, into FHIR resources, with
mappings that an implementation guide can publish. It works
with HAPI FHIR, Medplum, Aidbox, the Google Cloud Healthcare API and other
FHIR R4 servers.

## Install

Download the binary for your platform from the
[releases](https://github.com/ohs-foundation/fhir-pipes-rs/releases). The
current release is v0.6.0.

| Platform | File |
|---|---|
| macOS, Apple Silicon | `fhir-pipes-aarch64-apple-darwin` |
| macOS, Intel | `fhir-pipes-x86_64-apple-darwin` |
| Linux, x86_64 (static) | `fhir-pipes-x86_64-unknown-linux-musl` |
| Linux, aarch64 (static) | `fhir-pipes-aarch64-unknown-linux-musl` |
| Windows, x86_64 | `fhir-pipes-x86_64-pc-windows-msvc.exe` |

Check the download against the release's `SHA256SUMS`:

```sh
curl -LO https://github.com/ohs-foundation/fhir-pipes-rs/releases/latest/download/fhir-pipes-aarch64-apple-darwin
curl -LO https://github.com/ohs-foundation/fhir-pipes-rs/releases/latest/download/SHA256SUMS
shasum -a 256 -c SHA256SUMS --ignore-missing     # sha256sum -c … on Linux
chmod +x fhir-pipes-aarch64-apple-darwin && mv fhir-pipes-aarch64-apple-darwin /usr/local/bin/fhir-pipes
```

Or build it with Rust: `cargo install --git https://github.com/ohs-foundation/fhir-pipes-rs --locked fhir-pipes`.

The DHIS2 connector is a separate program, `fhir-pipes-dhis2`, in the same
release (`fhir-pipes-dhis2-<target>`). Install it as `fhir-pipes-dhis2` in
the folder of `fhir-pipes` or on the `PATH`, and `fhir-pipes dhis2 …` runs
it; or build it with
`cargo install --git https://github.com/ohs-foundation/fhir-pipes-rs --locked fhir-pipes-dhis2`.
See [docs/dhis2.md](docs/dhis2.md#installing-fhir-pipes-dhis2).

`--server-kind`, `--token-auth` and `--scope` need v0.6.0 or later.

## Quickstart: a SQL-on-FHIR warehouse

`refresh` downloads the resources a directory of ViewDefinitions reads, runs
the views, and writes each view as a Hive-partitioned Parquet table with a
manifest.

```toml
# fhir-pipes.toml
data = "warehouse"
views = "views"          # a directory of ViewDefinition JSON

[server]
base = "https://fhir.example.org/fhir"
name = "prod"
```

```sh
fhir-pipes refresh --check           # settings, steps and tools found
fhir-pipes refresh --only views      # download, run the views, write the tables
```

Query the tables with anything that reads Parquet, such as DuckDB
(`read_parquet('warehouse/parquet/patient_flat/*/*.parquet', hive_partitioning = true)`).

[docs/refresh.md](docs/refresh.md) covers the other steps, reading from
files, partitioning and hooks.

## Quickstart: importing from another system

An import has three parts:

1. A **connector** reads the source system and writes its records as NDJSON,
   one file per record type. `fhir-pipes dhis2 extract` is one. Any program
   in any language can be one.
2. An **import package** is a folder that says how the records become FHIR
   resources: a logical model of each record type and FHIR Mapping Language
   maps, with ViewDefinitions and SQL where records have to be joined or
   grouped. It holds no code, so an implementation guide can publish it.
3. **`fhir-pipes import run`** runs the package over the records and writes
   one NDJSON file per resource type, for `fhir-pipes load`.

DHIS2 org units, indicators and values from the open Sierra Leone demo, with
the example package [examples/import/icr-dhis2](examples/import/icr-dhis2):

```sh
export FHIR_PIPES_DHIS2_URL=https://play.im.dhis2.org/stable-2-42-7-1
export FHIR_PIPES_DHIS2_USER=admin FHIR_PIPES_DHIS2_PASSWORD=district

# 1. The connector writes records to raw/.
fhir-pipes dhis2 extract locations --instance sl-demo --id-prefix sle --country-code SLE --out raw
fhir-pipes dhis2 extract measures  --instance sl-demo --indicator-group srxUNt2w5jn --out raw
fhir-pipes dhis2 extract reports   --instance sl-demo --indicator-group srxUNt2w5jn \
  --period 2025 --level 2 --level 3 --out raw

# 2. Check the records against the package's logical models.
fhir-pipes import check --package examples/import/icr-dhis2 --in raw

# 3. Map them: fhir/Location.ndjson, Organization.ndjson, Measure.ndjson, MeasureReport.ndjson.
fhir-pipes import run --package examples/import/icr-dhis2 --in raw --out fhir

# 4. Send them to a FHIR server.
for t in Organization Location Measure MeasureReport; do
  fhir-pipes load --server URL --in fhir/$t.ndjson
done
```

The demo's address carries the DHIS2 version and changes with each release;
https://play.dhis2.org lists the current ones.

A package has parameters for the values that differ between deployments,
such as the base URL of identifier systems. `--param NAME=VALUE` sets one
for a run: `import run --param base=https://example.org`.

| To read about | See |
|---|---|
| Packages, `import.toml`, parameters, joining records with views and SQL, tests | [docs/import.md](docs/import.md) |
| Writing a connector, and running it as `fhir-pipes NAME` | [docs/connectors.md](docs/connectors.md) |
| The DHIS2 connector and its options | [docs/dhis2.md](docs/dhis2.md) |
| Writing and checking maps | [docs/maps.md](docs/maps.md) |

## Commands

Run `fhir-pipes <command> --help` for every option.

| Command | What it does | Docs |
|---|---|---|
| `refresh` | Build or refresh a SQL-on-FHIR Parquet warehouse | [refresh.md](docs/refresh.md) |
| `serve` | Run refreshes over HTTP or on a schedule; notify destinations; run approved SQLQuery Libraries (Unix only) | [serve.md](docs/serve.md) |
| `views run`, `sql`, `validate`, `lint`, `test` | Run ViewDefinitions over local files or PostgreSQL, generate SQL, check them | [views.md](docs/views.md) |
| `export` | Download resources as NDJSON by search, Bulk Data `$export` or from a HAPI database | [export.md](docs/export.md) |
| `sync` | Copy what `export` downloaded to a second FHIR server | [sync.md](docs/sync.md) |
| `extract`, `transform locations`, `run`, `inspect` | Snapshot Locations and Organizations; turn them into GeoParquet | [locations.md](docs/locations.md) |
| `diff`, `load` | Edited GeoJSON or GeoParquet to changed resources; send NDJSON to a server with version checks | [locations.md](docs/locations.md), [qgis.md](docs/qgis.md) |
| `import location-boundaries`, `import location-sites`, `index`, `population` | Build Locations, spatial indexes and population Groups offline | [locations.md](docs/locations.md) |
| `dhis2 extract`, `dhis2 locations`, `measures`, `reports` | DHIS2 org units, indicators and data as records for `import run` with [examples/import/icr-dhis2](examples/import/icr-dhis2); `locations`, `measures` and `reports` also map them with a package and write FHIR NDJSON. Needs the separate program `fhir-pipes-dhis2` | [dhis2.md](docs/dhis2.md) |
| `libraries build`, `libraries sql` | SQL on FHIR Libraries: build Library JSON from SQL files, print the SQL to run one | [libraries.md](docs/libraries.md) |
| `profile check` | Compare a profile with its IG | [profiles.md](docs/profiles.md#checking-a-profile-against-its-ig) |
| `terminology valueset`, `conceptmap` | A value set or concept map from FHIR packages as a table | [terminology.md](docs/terminology.md) |
| `map run`, `map check` | Run and type-check FHIR Mapping Language maps over NDJSON records | [maps.md](docs/maps.md) |
| `import check`, `import run`, `import test` | Run an import package (logical models, maps, views, Libraries) over a folder of records, to FHIR NDJSON | [import.md](docs/import.md) |
| `fhir-pipes NAME` | Run a separate program `fhir-pipes-NAME`, such as a connector that writes records for `import run` | [connectors.md](docs/connectors.md) |

An implementation guide's extra Location and Organization columns come from a
profile (`--profile`, `FHIR_PIPES_PROFILE` or `profile =` in
`fhir-pipes.toml`); without one only base FHIR is read. See
[docs/profiles.md](docs/profiles.md).

Every command exits 0 on success, 2 for an error you can fix before running
again (a bad flag, a missing or invalid config, profile, view or input file),
and 1 for a failure while it runs (network, server, database, a tool, a disk
write). `views validate`, `views lint`, `views test` and `profile check` exit
1 when they find problems.

## Supported FHIR servers

Any FHIR R4 server with search on `_lastUpdated`, `Bundle.link` paging and
transaction bundles works, read by paged search or Bulk Data `$export`
(`--bulk`); HAPI can also be read from its database. `--server-kind`
(`generic` by default, `hapi`, `medplum`, `aidbox`; for `refresh` also
`FHIR_PIPES_SERVER_KIND` or `server.kind`) selects the search parameters a
server needs. Aidbox has not been checked against a real server. Setup for
each server is in [docs/servers.md](docs/servers.md).

## Connecting to a FHIR server

A server that needs authentication gets one of four kinds. They are mutually
exclusive; with none, requests carry no Authorization header.

- `--token` (or `FHIR_PIPES_TOKEN`): a fixed bearer token. A run that outlasts
  it fails with a 401.
- `--client-id`, `--client-secret`, `--token-url` (or `FHIR_PIPES_CLIENT_ID`,
  `FHIR_PIPES_CLIENT_SECRET`, `FHIR_PIPES_TOKEN_URL`): an OAuth client using
  the client-credentials grant. The token is renewed a minute before it
  expires. `--token-auth post` (default) or `basic` says how the client
  ID and secret reach the token endpoint; `--scope` adds a scope to the request.
  `refresh` reads the client ID and secret only from the environment.
- `--username` and `--password` (or `FHIR_PIPES_USERNAME`,
  `FHIR_PIPES_PASSWORD`): HTTP Basic auth. Use HTTPS, since Basic sends the
  password with every request. `refresh` reads the password only from the
  environment.
- `--google-auth` (or `FHIR_PIPES_GOOGLE_AUTH=1`): Google Application Default
  Credentials, for a Cloud Healthcare API FHIR store.

`--timeout` (300 seconds) and `--retries` set the deadline and the retries
for one HTTP request; 429 and 5xx responses are retried, other 4xx are final.

### Google Cloud Healthcare API

`--server` also takes a FHIR store path,
`projects/<p>/locations/<l>/datasets/<d>/fhirStores/<s>`. With `--google-auth`,
credentials come from `GOOGLE_APPLICATION_CREDENTIALS`, gcloud's application
default credentials, or the metadata server on Google Cloud, in that order.
The account needs `roles/healthcare.fhirResourceReader` to read and
`roles/healthcare.fhirResourceEditor` to write (`load`, `sync`).

## Using fhir-pipes with an AI agent

[skills/fhir-pipes](skills/fhir-pipes) is an [Agent Skill](https://agentskills.io)
that tells a coding agent how to run fhir-pipes safely. Copy or symlink it into
the agent's skills directory, such as `~/.claude/skills/`.

## Building and testing

Requires a stable Rust toolchain.

```sh
cargo build --release --workspace     # fhir-pipes and fhir-pipes-dhis2
cargo test --workspace
```

The tests of `crates/dhis2` also run `fhir-pipes`, which `cargo test
--workspace` builds beside `fhir-pipes-dhis2`.

Two Cargo features add optional parts: `postgres` (on in the release
binaries; `views run --db` and `refresh` with `[database]`) and
`fhir-packages` (off; `views lint --package`). Build with
`--features fhir-packages` for everything, or `--no-default-features` without
PostgreSQL.

## License

Apache-2.0. `crates/sof` and `crates/sof-lint` are MIT OR Apache-2.0, from
octofhir/sof by the OctoFHIR Team.
