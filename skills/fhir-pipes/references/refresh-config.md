# refresh and fhir-pipes.toml

`fhir-pipes refresh` downloads the resource types a directory of
ViewDefinitions reads, runs the views, and writes each view as a
Hive-partitioned Parquet table, with a manifest. It reads `--config`, else
`./fhir-pipes.toml` (or `./fhir-warehouse.toml`). Flags beat environment
variables, which beat the file. Paths in the file are relative to the file.

Always run `fhir-pipes refresh --check` first. It prints the steps, server,
kind, auth kind (never secrets), fetch mode, paths, profile and the tools it
found, and changes nothing.

## Steps

| Step | Does | Needs |
|---|---|---|
| `locations` | extract Locations into `raw/<name>/locations`, transform to `parquet/locations` | |
| `views` | download the types the views read (incrementally), run the views, write tables | |
| `tiles` | `tiles/*.pmtiles` from the Location table | duckdb (spatial), tippecanoe |
| `manifest` | `manifest.json` | |
| `catalog` | `catalog.sql` (DuckDB views), optional `catalog.spark.sql`; only with `[catalog]` | duckdb only for `duckdb_file` |
| `database` | the view tables into PostgreSQL; only with `[database]` | |
| `hooks` | `[[hooks]]` commands, in order | the commands |
| `push` | `rclone sync`; only with `--push` | rclone |

By default every step except `push` runs. `--only views` picks steps; the
manifest, catalog and database are added when a data step is picked, and
every step not picked, including `hooks`, is left out.
`--skip tiles` leaves one out. The tiles step expects ICR-shaped columns;
with another profile, use `--skip tiles`.

A run that fails before the publish step leaves the previous tables,
`views/`, `tiles/`, `manifest.json` and `catalog.sql` as they were; only
`raw/` moves forward. The publish step swaps in the new tables, manifest and
`catalog.sql`, then builds `catalog.duckdb`, and the hooks and push run after
it. A failure from then on (the `catalog.duckdb` build, a hook, the push)
exits 1 with the new tables and manifest already published; a failed
`catalog.duckdb` build keeps the previous `catalog.duckdb`. The run's output
names the step that failed.

## Output layout

```
<data>/
  raw/<name>/<Type>.ndjson         the downloads; .export-state.json holds the watermarks
  raw/<name>/locations/            the Location snapshot
  raw/<name>/last-refresh.json     step timings and type mismatches of the last good run
  parquet/<table>/<partition>=<value>/part-0.parquet
  views/<name>.json                the views that were run
  manifest.json                    time, server, row count per table, views
  catalog.sql                      with [catalog]
  tiles/                           with the tiles step
```

A table is the view's name in snake case, without any `[tables] strip_prefix`.
Query with DuckDB:

```sql
SELECT * FROM read_parquet('warehouse/parquet/patient_flat/*/*.parquet', hive_partitioning = true);
```

## Settings

| Config | Flag / env | Default |
|---|---|---|
| `data` | `--data`, `FHIR_PIPES_DATA` | required |
| `views` | `--views`, `FHIR_PIPES_VIEWS` | required for `views` |
| `profile` | `--profile`, `FHIR_PIPES_PROFILE` | none |
| `server.base` | `--server`, `FHIR_PIPES_SERVER` | `http://localhost:3447/fhir` |
| `server.name` | `--server-name`, `FHIR_PIPES_SERVER_NAME` | `hapi-local`; `files` with a file source |
| `server.kind` | (no flag) | `generic`; `hapi`, `medplum`, `aidbox` |
| `server.fetch` | | `search`; `bulk`; `hapi-db` |
| `server.bulk_timeout` | | 14400 |
| `server.concurrency` | | 4 |
| `server.token_url` | `--token-url`, `FHIR_PIPES_TOKEN_URL` | |
| `server.token_auth` | `--token-auth`, `FHIR_PIPES_TOKEN_AUTH` | `post` |
| `server.scope` | `--scope`, `FHIR_PIPES_SCOPE` | |
| `server.username` | `--username`, `FHIR_PIPES_USERNAME` | |
| `server.google_auth` | `--google-auth`, `FHIR_PIPES_GOOGLE_AUTH` | false |
| `source.files` | `--input` (repeat) | none: read from the server |
| `tiles.countries` | `--countries` | every country |

Secrets that stay in the environment only: `FHIR_PIPES_TOKEN`,
`FHIR_PIPES_CLIENT_ID`, `FHIR_PIPES_CLIENT_SECRET`, `FHIR_PIPES_PASSWORD`,
`FHIR_PIPES_HAPI_DATABASE_URL`, and the database URL variable named by
`[database] url_env`. Never write them into the file.

Unknown keys are refused with exit 2.

## Example

```toml
data = "warehouse"
views = "views"

[server]
base = "https://fhir.example.org/fhir"
name = "prod"
kind = "generic"            # "medplum" or "aidbox" for those servers
fetch = "search"            # "bulk" for $export, "hapi-db" for HAPI's database
concurrency = 4
# token_url = "https://auth.example.org/realms/x/protocol/openid-connect/token"
# token_auth = "basic"      # Aidbox
# google_auth = true

[database]
url_env = "FHIR_PIPES_DATABASE_URL"
schema = "public"
# tables = ["indicator_value"]   # default: every view table

[catalog]
base_url = "https://data.example.org/warehouse"

[[hooks]]
name = "notify"
after = "views"
run = ["./notify.sh"]
```

## From files

`refresh --input exports/` (or `[source] files = [...]`) reads NDJSON,
Bundles, JSON resources, directories or globs instead of a server. The
server is never contacted. The resources are split into `raw/<name>/`.
`--reuse-raw` reruns on that split.

## Bulk and HAPI database

- `fetch = "bulk"`: system-level `$export` for the views step. A refresh
  that stops while the server exports polls the same export next time.
  `--full` exports without `_since`. The `locations` step still uses search.
- `fetch = "hapi-db"`: HAPI's PostgreSQL database, from
  `FHIR_PIPES_HAPI_DATABASE_URL`. HAPI only. The `locations` step still
  uses search, so `server.base` is still needed for it.

## PostgreSQL

With `[database]`, the database step loads every view table (not the
Location table) in one transaction: create if missing, `DELETE`, `COPY`.
Readers see the old rows until the commit. The connection string comes
only from the variable `url_env` names (default `FHIR_PIPES_DATABASE_URL`).
Its `sslmode` sets TLS; managed databases usually need `require` or more.
A build without the `postgres` feature stops with exit 2; `--skip database`
runs the rest.

Before loading into a database, confirm with the user which database it
is. Do not load into a production database unless the user says so.
