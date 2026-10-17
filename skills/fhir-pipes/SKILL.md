---
name: fhir-pipes
description: Run the fhir-pipes command-line tool to move data into and out of FHIR R4 servers. Use when the user wants to export or extract FHIR resources to NDJSON from HAPI FHIR, Medplum, Aidbox, Google Cloud Healthcare API or another FHIR server (paged search, Bulk Data $export, or HAPI's PostgreSQL database), build SQL-on-FHIR ViewDefinitions into Parquet tables, refresh a warehouse with fhir-pipes.toml, load view tables into PostgreSQL, turn Location registries and admin boundaries into GeoParquet or map tiles, load or sync resources into a FHIR server, import DHIS2 data, work with fhir-pipes profiles, or explain a fhir-pipes error or exit code.
license: Apache-2.0
compatibility: Needs the fhir-pipes binary (release v0.5.0 or a build from main) and a shell. Network access only for the FHIR servers, databases and DHIS2 instances the user names.
metadata:
  repository: https://github.com/ohs-foundation/fhir-pipes-rs
---

# fhir-pipes

fhir-pipes is one binary with no runtime dependencies. It downloads FHIR
resources as NDJSON, runs SQL-on-FHIR ViewDefinitions into Parquet, builds
and refreshes a warehouse, turns Locations into GeoParquet, and sends
resources back to a FHIR server. This skill says which command to use, how
to run it safely, and where its output goes. Get the exact flags from
`fhir-pipes <command> --help` or [references/commands.md](references/commands.md).
Do not rely on memory for them.

## Before running anything

1. Check that the binary is there and which version it is:
   `fhir-pipes --version`. If it is missing, see Install below.
2. `--server-kind`, `--token-auth` and `--scope` were added after v0.5.0.
   If `fhir-pipes export --help` does not list them, the binary is too old
   for those flags. Build it from main or wait for the next release.
3. Ask which server it is (HAPI, Medplum, Aidbox, Google Cloud Healthcare
   API, other), its FHIR base URL, and how it authenticates. Do not guess a
   base URL.
4. Ask the user to set secrets in the environment themselves. Do not ask
   them to paste a secret into the chat. See Safety rules.

## Install

Release binaries are on https://github.com/ohs-foundation/fhir-pipes-rs/releases
(current release v0.5.0). There are five files: `fhir-pipes-aarch64-apple-darwin`,
`fhir-pipes-x86_64-apple-darwin`, `fhir-pipes-x86_64-unknown-linux-musl`,
`fhir-pipes-aarch64-unknown-linux-musl`, `fhir-pipes-x86_64-pc-windows-msvc.exe`.
Always check the download against `SHA256SUMS`:

```sh
curl -LO https://github.com/ohs-foundation/fhir-pipes-rs/releases/latest/download/fhir-pipes-aarch64-apple-darwin
curl -LO https://github.com/ohs-foundation/fhir-pipes-rs/releases/latest/download/SHA256SUMS
shasum -a 256 -c SHA256SUMS --ignore-missing      # Linux: sha256sum -c SHA256SUMS --ignore-missing
chmod +x fhir-pipes-aarch64-apple-darwin && mv fhir-pipes-aarch64-apple-darwin /usr/local/bin/fhir-pipes
fhir-pipes --version
```

From source (needs Rust): `cargo install --git https://github.com/ohs-foundation/fhir-pipes-rs --locked fhir-pipes`.

## Choosing a command

| The user wants to | Command |
|---|---|
| Download resources of some types as NDJSON | `export` |
| A Parquet warehouse from ViewDefinitions, kept up to date | `refresh` (config in `fhir-pipes.toml`) |
| Run one view or a folder of views over local files, or see its SQL | `views run`, `views sql` |
| Check or test ViewDefinitions | `views validate`, `views lint`, `views test` |
| View tables in PostgreSQL | `refresh` with a `[database]` section |
| The Location registry as GeoParquet | `run` (extract + transform), or `extract` then `transform locations` |
| Send edited Locations back after GIS work | `diff`, then `load` |
| Admin boundaries (GeoJSON) or sites (CSV) as Locations | `import location-boundaries`, `import location-sites`, then `load` |
| Copy an export to a second FHIR server | `sync` |
| DHIS2 org units, indicators or values as FHIR | `dhis2 locations`, `dhis2 measures`, `dhis2 reports` |
| Check a fhir-pipes profile against its IG | `profile check` |

## Choosing `--server-kind`

`export`, `extract` and `run` take `--server-kind`. `refresh` has no flag
for it; set `kind` in `[server]` in `fhir-pipes.toml`.

| Server | Kind | Notes |
|---|---|---|
| Any FHIR R4 server | `generic` (default) | |
| HAPI FHIR | `generic` or `hapi` (the same) | `--bulk` needs `hapi.fhir.bulk_export_enabled=true`. Only HAPI can be read from its database (`--hapi-db`). |
| Google Cloud Healthcare API | `generic` | `--google-auth`; `--server` can be `projects/P/locations/L/datasets/D/fhirStores/S` |
| Medplum | `medplum` | Without it, search stops at about 11,000 resources per type |
| Aidbox | `aidbox` | Base is `<aidbox>/fhir`, not the root |

Auth and per-server setup: [references/servers-and-auth.md](references/servers-and-auth.md).

## Search, bulk or database; incremental or full

- **Search** (default): paged `_lastUpdated` searches. Works on every server.
- **`--bulk`**: a system-level Bulk Data `$export`. Use it for large
  servers that support it. `--bulk-timeout` (14400 s) is how long to wait.
- **`--hapi-db`**: reads a HAPI JPA server's PostgreSQL database. HAPI only.
  The connection string comes only from `FHIR_PIPES_HAPI_DATABASE_URL`. It
  depends on HAPI's internal schema, so prefer search or `--bulk` unless
  they are too slow.
- **`--incremental`**: downloads only what changed since the last run into
  the same `--out` (from `OUT/.export-state.json`) and merges by id. If a
  type's count then differs from the server's, that type is downloaded in
  full. Without `--incremental`, every type is downloaded in full.
  `refresh` is incremental unless given `--full`.

## Workflows

### Export Patients and Observations from Medplum, incrementally

```sh
# The user sets the secret in their own shell first:
#   export FHIR_PIPES_CLIENT_SECRET=...
export FHIR_PIPES_CLIENT_ID=<ClientApplication id>
fhir-pipes export --server https://api.medplum.com/fhir/R4 --server-kind medplum \
  --token-url https://api.medplum.com/oauth2/token \
  --incremental --type Patient --type Observation --out raw/
```

The output is `raw/Patient.ndjson`, `raw/Observation.ndjson` and
`raw/.export-state.json`. Run the same command again for the next increment.
Add `--bulk` for `$export`. The ClientApplication's AccessPolicy must then
also allow `AsyncJob` and `Binary`.

### Build a warehouse and load the view tables into PostgreSQL

```toml
# fhir-pipes.toml
data = "warehouse"
views = "views"                  # directory of ViewDefinition JSON

[server]
base = "https://fhir.example.org/fhir"
name = "prod"
kind = "generic"

[database]
url_env = "FHIR_PIPES_DATABASE_URL"   # the variable that holds the connection string
schema = "public"
```

```sh
# The user sets FHIR_PIPES_DATABASE_URL (postgres://…?sslmode=require) themselves.
fhir-pipes refresh --check             # resolved settings, steps, tools found; changes nothing
fhir-pipes refresh --only views        # download, run views, write Parquet, load PostgreSQL
```

`--only views` also adds the manifest, and the catalog and database steps
when their sections exist. Every other step (locations, tiles, hooks, push)
is left out. The output goes to `warehouse/parquet/<table>/<partition>=<value>/part-0.parquet`,
`warehouse/manifest.json`, and the downloads to `warehouse/raw/<name>/`. To
use local files instead of a server: `fhir-pipes refresh --input exports/`.
Details: [references/refresh-config.md](references/refresh-config.md).

### Run views over local files, no warehouse

```sh
fhir-pipes views validate views/patient_flat.json
fhir-pipes views run views/patient_flat.json --input raw/Patient.ndjson            # CSV to stdout
fhir-pipes views run views/ --input raw/ --output parquet --out tables/
```

### Export from HAPI's database

```sh
# The user sets FHIR_PIPES_HAPI_DATABASE_URL to a read-only role's connection string.
fhir-pipes export --hapi-db --incremental --type Patient --type Observation --out raw/
```

### Edit Locations in a GIS tool and send them back

```sh
fhir-pipes run --server https://fhir.example.org/fhir --snapshot snapshot/ --out out/ --profile <profile.toml>
# … the user edits out/locations/... in QGIS and saves edits.geojson …
fhir-pipes diff --snapshot snapshot/ --in edits.geojson --out changes.ndjson
fhir-pipes load --server https://fhir.example.org/fhir --in changes.ndjson --dry-run
# only after the user has read the plan and said to go ahead:
fhir-pipes load --server https://fhir.example.org/fhir --in changes.ndjson
```

`import`, `index`, `population` and `dhis2` need a profile (`--profile` or
`FHIR_PIPES_PROFILE`). See [references/profiles.md](references/profiles.md).

## Safety rules

- **Secrets only in the environment.** Tokens, client secrets, passwords
  and database URLs go in environment variables (`FHIR_PIPES_TOKEN`,
  `FHIR_PIPES_CLIENT_SECRET`, `FHIR_PIPES_PASSWORD`,
  `FHIR_PIPES_HAPI_DATABASE_URL`, `FHIR_PIPES_DATABASE_URL`,
  `FHIR_PIPES_DHIS2_TOKEN`, `FHIR_PIPES_DHIS2_PASSWORD`), or in the
  variable a config names (`[database] url_env`). The user sets them in
  their own shell. Never put a secret on the command line (`--token`,
  `--client-secret`, `--password`, `--dhis2-token` with a value), in
  `fhir-pipes.toml`, in a script, or in any file you write. Never print,
  echo, log or read back a secret's value. To check that one is set, use
  `[ -n "$FHIR_PIPES_CLIENT_SECRET" ] && echo set`.
- **Look before writing.** Run `refresh --check` before a refresh,
  `load --dry-run` before `load`, `sync --dry-run` before `sync`, and
  `refresh --only push --dry-run` before a push. Show the user the result.
  `refresh --push --dry-run` is not a preview: it runs the whole refresh
  (views, database, publish, hooks) and only the upload is a dry run.
- **Production needs the user's say-so.** Do not run `load` or `sync`
  against a production FHIR server, or a `refresh` with `[database]`
  against a production database, unless the user has said that is the
  target. When unsure, ask.
- **Deletions.** `sync --delete` deletes from the sink. Never add
  `--force-delete` unless the user asks for it. It overrides the
  `--max-delete-share` guard (0.2).
- **The direct database export is HAPI only.** `--hapi-db` (and
  `fetch = "hapi-db"`) reads HAPI's tables. It does not work for Medplum,
  Aidbox or any other server; use search or `--bulk` for those. Use a
  read-only database role.
- **Do not run against servers the user did not name.** Placeholder URLs
  such as `fhir.example.org` are examples. Never send requests to them.

## Exit codes and where to look next

- `0`: success.
- `2`: an error the user can fix before running again: a bad flag, a
  missing or invalid config file, profile, ViewDefinition or input file, a
  missing profile, or a feature the build lacks. Fix the input; do not
  retry unchanged.
- `1`: a failure while running: the network, the server, the database, a
  tool (duckdb, tippecanoe, rclone), a hook, a disk write. Also used when
  `views validate`, `views lint`, `views test` or `profile check` find
  problems.

The message on stderr starts with `fhir-pipes:`. For common messages and
fixes see [references/troubleshooting.md](references/troubleshooting.md).

## References

- [references/commands.md](references/commands.md): every command and subcommand, key flags and defaults.
- [references/servers-and-auth.md](references/servers-and-auth.md): auth options and environment variables, setup for HAPI, Google, Medplum and Aidbox.
- [references/refresh-config.md](references/refresh-config.md): `fhir-pipes.toml`, steps, outputs, file sources, bulk, HAPI database, PostgreSQL, catalog.
- [references/profiles.md](references/profiles.md): what a profile is, which commands need one, `profile check`.
- [references/troubleshooting.md](references/troubleshooting.md): exit codes, common errors and fixes.
