# Commands and flags

The binary has the same commands and flags in every build. Two options need
a Cargo feature: `views run --db` needs `postgres` (in the release binaries),
and `views lint --package` needs `fhir-packages` (not in the release
binaries). Without the feature the command exits 2 and names it.

`--profile FILE|URL` (or `FHIR_PIPES_PROFILE`) is accepted by every command.
Most commands read and write an NDJSON path ending in `.gz` as gzip. The
exceptions:
`views run --input FILE.gz` reads the compressed bytes and fails, `views run
--out FILE.gz` writes plain NDJSON, and directory inputs for `views` and
`refresh --input` skip `.gz` files. For those, decompress first (`gunzip -k`)
and compress the output yourself. Run
`fhir-pipes <command> --help` for the full text; this file lists what is
needed to pick flags.

## Shared FHIR client flags

`extract`, `export`, `run`, `load` and `sync` take these:

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--server URL` | | (required; `export` may omit it with `--hapi-db`) | FHIR base URL, or a Google store path `projects/P/locations/L/datasets/D/fhirStores/S` |
| `--token` | `FHIR_PIPES_TOKEN` | | fixed bearer token |
| `--client-id` | `FHIR_PIPES_CLIENT_ID` | | OAuth client credentials; fhir-pipes gets and renews its own token |
| `--client-secret` | `FHIR_PIPES_CLIENT_SECRET` | | use the env var, not the flag |
| `--token-url` | `FHIR_PIPES_TOKEN_URL` | | OAuth token endpoint |
| `--token-auth post\|basic` | `FHIR_PIPES_TOKEN_AUTH` | `post` | `basic` puts id and secret in a Basic header (Aidbox). v0.6.0 and later |
| `--scope` | `FHIR_PIPES_SCOPE` | none | e.g. `system/*.read`. v0.6.0 and later |
| `--username` | `FHIR_PIPES_USERNAME` | | HTTP Basic |
| `--password` | `FHIR_PIPES_PASSWORD` | | use the env var, not the flag |
| `--google-auth` | `FHIR_PIPES_GOOGLE_AUTH` | off | Google Application Default Credentials |
| `--retries` | | 3 | attempts per request (429, 5xx, connection errors) |
| `--timeout` | | 300 | seconds per HTTP request |

Only one auth kind can be used at a time. With none, requests carry no
Authorization header.

## refresh

`fhir-pipes refresh [OPTIONS]`: build or refresh a SQL-on-FHIR Parquet
warehouse from `fhir-pipes.toml` (or `--config`, `FHIR_PIPES_CONFIG`).

| Flag | Meaning |
|---|---|
| `--check` | print resolved settings, steps and tools found, then exit; changes nothing |
| `--only STEPS`, `--skip STEPS` | comma-separated: `locations, views, tiles, manifest, catalog, database, hooks, push` |
| `--full` | download everything again instead of only changes |
| `--reuse-raw` | rerun on the last download in `raw/<name>/` |
| `--input PATH` (repeat) | read NDJSON/Bundle/JSON files, directories or globs instead of a server |
| `--push`, `--dry-run` | also push with rclone; `--dry-run` makes only the upload a dry run, so use `--only push --dry-run` to preview a push without running the other steps |
| `--data`, `--views`, `--server`, `--server-name` | override the config (`FHIR_PIPES_DATA`, `FHIR_PIPES_VIEWS`, `FHIR_PIPES_SERVER`, `FHIR_PIPES_SERVER_NAME`) |
| `--server-kind generic\|hapi\|medplum\|aidbox` | overrides `server.kind` (`FHIR_PIPES_SERVER_KIND`) |
| `--token`, `--token-url`, `--token-auth`, `--scope`, `--username`, `--google-auth` | auth; client id, client secret and password come only from the environment |
| `--countries` | countries to build tiles for |
| `--threads` | default: number of CPUs |

`refresh --server-kind` and `FHIR_PIPES_SERVER_KIND` are newer than the
other kind flags; on a binary without them, use `server.kind` in the config.
See [refresh-config.md](refresh-config.md).

## views

All `views` subcommands work offline on files, except `run --db`.

- `views run VIEW --input PATH [--output csv|ndjson|json|parquet] [--out FILE|DIR] [--parquet-temporal text|native] [--threads N]`.
  VIEW is a ViewDefinition JSON file or a directory of them; a directory
  writes one file per view into `--out`. `--input` is NDJSON, a Bundle, a
  JSON resource or array, a directory, or `-` for stdin. `--db URL` (or
  `FHIR_PIPES_DATABASE_URL`) runs against PostgreSQL with FHIR JSONB instead.
  Default output is CSV on stdout.
- `views sql VIEW [--dialect ansi|postgres|duckdb] [--ddl] [--table NAME]`:
  the SELECT for a view (PostgreSQL by default), or `--ddl` for a CREATE TABLE.
- `views validate VIEW [--json|--sarif]`: structure checks; exit 1 on an error.
- `views lint VIEW [--shareable] [--allow-fn NAME] [--package hl7.fhir.r4.core [--version V]] [--json|--sarif]`:
  FHIRPath checks; exit 1 on an error.
- `views test MANIFEST`: SQL-on-FHIR test-case files (a file or directory);
  exit 1 on any failure.

In Parquet, a value that does not fit its column's type is written as null
and counted on stderr; the run does not fail.

## export

`fhir-pipes export [AUTH] --server URL --type TYPE [--type TYPE …] --out DIR`

| Flag | Default | Meaning |
|---|---|---|
| `--server-kind generic\|hapi\|medplum\|aidbox` | `generic` | search parameters the server needs. v0.6.0 and later |
| `--incremental` | off | only what changed since the last run into `--out`, merged by id |
| `--bulk` | off | system-level Bulk Data `$export` instead of search |
| `--bulk-timeout SECS` | 14400 | wait for the export, then cancel it; also per-file download limit |
| `--hapi-db` | off | read HAPI's PostgreSQL database; URL only from `FHIR_PIPES_HAPI_DATABASE_URL` |
| `--concurrency N` | 4 | search requests in flight; with `--bulk`, files downloaded at once; unused with `--hapi-db` |
| `--report FILE` | | the run's report as JSON |

Output: `DIR/<TYPE>.ndjson` per type and `DIR/.export-state.json`. Files are
replaced only when the type's download completes.

## extract, transform locations, run, inspect

- `extract [AUTH] --server URL --snapshot DIR [--server-kind KIND] [--full] [--since INSTANT] [--concurrency 8] [--max-consecutive-failures 50] [--no-cache] [--refresh] [--cache-dir DIR]`:
  Locations and Organizations into a snapshot (`locations.ndjson`,
  `organizations.ndjson`, `state.json`, `boundaries/`), incrementally.
- `transform locations --snapshot DIR --out DIR [--country CC] [--row-group-size 20000] [--partition-by country,geom_type,type] [--keep-without-geometry]`:
  GeoParquet in `OUT/locations/`. Offline.
- `run`: the flags of both; extract then transform.
- `inspect --out DIR`: summarise a written dataset.

## diff, load, index

- `diff --snapshot DIR --in EDITS --out CHANGES.ndjson [--report FILE]`:
  edited `.geojson`, `.geojsonl` or `.parquet` compared with the snapshot;
  writes the changed Locations. Offline.
- `load [AUTH] --server URL --in FILE.ndjson [--dry-run] [--batch-size 100] [--concurrency 1]`:
  transaction Bundles of `PUT`, parents first, with `ifMatch` version
  checks. `--dry-run` runs the preflight and prints the plan.
- `index --snapshot DIR --spatial-index quadkey:18 [--spatial-index geohash:8] --out CHANGES.ndjson [--refresh] [--report FILE]`:
  spatial index cells onto Locations that lack them. Needs a profile.

## sync

`sync [AUTH] --server SINK --from DIR [--type TYPE …] [--full] [--delete [--max-delete-share 0.2] [--force-delete]] [--dry-run] [--batch-size 100] [--concurrency 1]`

Copies what `export` wrote in DIR to a second server, only what changed
since the last sync to it. The auth options are the sink's. State is in
`DIR/.sync-state.json` and `DIR/.sync/`.

## import, population

All offline, and all need a profile.

- `import location-boundaries --in FILE.geojson --country NAME=CODE --level LEVEL=NAME_PROP[:CODE_PROP] … --out UNITS.ndjson [--alias LEVEL=PROP] [--code-system URI] [--dissolve-parents] [--report FILE]`
- `import location-sites --in SITES.csv --boundaries UNITS.ndjson --type CODE --name-col C --lat-col C --lon-col C --id-col C --parent LEVEL=COLUMN … --out SITES.ndjson [--paired-org] [--identifier SYSTEM=COL] [--where COL=VALUE] [--spatial-index quadkey:18]`
- `population --snapshot DIR --raster PATH_OR_URL (--level N | --type CODE) --year YYYY --out GROUPS.ndjson [--source worldpop] [--planning-denominator] [--estimate-date YYYY-MM-DD] [--cache-dir DIR] [--timeout 3600]`

## dhis2

All need a profile with a `[dhis2]` section. Nothing is written to DHIS2.
Auth: `--dhis2-token` (`FHIR_PIPES_DHIS2_TOKEN`) or `--dhis2-user` and
`--dhis2-password` (`FHIR_PIPES_DHIS2_USER`, `FHIR_PIPES_DHIS2_PASSWORD`);
`--dhis2 URL` (`FHIR_PIPES_DHIS2_URL`).

- `dhis2 locations --dhis2 URL --instance NAME --out FILE [--country-code CC] [--facility-level N] [--root UID] [--id-prefix P]`
- `dhis2 measures --dhis2 URL --instance NAME --out FILE (--indicator-group UID | --indicator UID | --data-element-group UID | --data-element UID | --data-element-group-set UID | --data-set UID) …`
- `dhis2 reports --dhis2 URL --instance NAME --locations LOCATIONS.ndjson --level N … (--period P … | --range START:END …) (--indicator-group UID | --indicator UID | --data-element-group UID | --data-element UID | --data-element-group-set UID | --data-set UID) … --out FILE [--source analytics|data-values] [--previous FILE --changed FILE --removed FILE] [--resume] [--concurrency 4]`: needs at least one of the same selection flags as `measures`.

## profile check

`profile check --profile FILE|URL --ig PACKAGE_OR_DIR`: compare a profile
with its IG (a `package.tgz` path or https URL, or a directory of JSON
resources). Exit 1 on errors.
