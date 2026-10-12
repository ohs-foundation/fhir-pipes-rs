# fhir-warehouse

Refreshes a parquet warehouse from a FHIR server: one binary that downloads the
data, runs [SQL-on-FHIR](https://sql-on-fhir.org) ViewDefinitions over it, and
writes the result as hive-partitioned parquet tables with a manifest. It can
also build map tiles of the location registry, run your own commands
afterwards, and mirror the warehouse to object storage.

The ViewDefinitions run in process with
[octofhir-sof](https://github.com/recodelabs/sof) on every CPU, each resource
type read once however many views use it. The FHIR side goes through
[kiln](https://github.com/recodelabs/kiln); tiles use duckdb and tippecanoe;
pushing uses rclone. Only the tools the chosen steps need must be installed.

## Install

```bash
cargo install --git https://github.com/recodelabs/fhir-warehouse
```

## Run

```bash
fhir-warehouse refresh --config fhir-warehouse.toml
fhir-warehouse refresh --only views --reuse-raw          # views from the last download
fhir-warehouse refresh --server https://example.org/fhir --server-name prod --push
fhir-warehouse check                                      # settings, steps, tools found
```

### Steps

| Step | What it does | Needs |
|---|---|---|
| `locations` | `kiln run`: the Location registry, incrementally, into `raw/<server>/locations` and `parquet/locations` | kiln |
| `views` | `kiln export` every resource type the views read (one call), run each type's views on one read of it, write each view's table | kiln (none with `--reuse-raw`) |
| `tiles` | the registry → `tiles/admin.pmtiles`, `facilities`, `settlements`, `catchments` | duckdb (spatial), tippecanoe |
| `manifest` | `manifest.json`: time, server, each table's row count, the views, kiln's extract state | |
| `hooks` | the config's commands, in order | the commands |
| `push` | `rclone sync` of the configured directories and files; only with `--push` | rclone |

`locations` runs first. `views` and `tiles` then run side by side, since both
read only the registry. The manifest, the hooks and the push follow.

By default every step except `push` runs. `--only views,tiles` picks steps (the
manifest is added when a data step is picked), `--skip tiles` leaves some out,
and `--push` adds the push. Before anything runs, the binary checks that each
tool the chosen steps need is installed, and stops if one is missing.

### Tables

Each ViewDefinition writes `parquet/<table>/<column>=<value>/part-0.parquet`.
The table is the view's name in snake case minus a configured prefix
(`IcrIndicatorValue` → `indicator_value`). Dates, dateTimes, instants and times
are parquet date and timestamp types, not text.

Every row gets a partition column (default `country`), looked up through
another table: the first `[[partition.lookup]]` rule whose `key` column the view
has gives the table and column to look the value up in. A row without a match
goes to the null partition, `__HIVE_DEFAULT_PARTITION__`, as hive and DuckDB
name it. Tables other views look up in are written first. A table is replaced
only once all of its new files are written.

The views directory can be any directory of JSON files: files whose
`resourceType` is `ViewDefinition`, or ends in `/ViewDefinition` as in an IG
build (`fsh-generated/resources/Binary-*.json`), are views; the rest are
skipped. Each view is checked against the spec's structure rules before
anything runs, and a plain copy is saved to `views/<name>.json`.

## Settings

Command-line flags win over their environment variables, which win over the
config file (`--config`, else `./fhir-warehouse.toml` when present). Paths in
the file are relative to the file.

| Flag | Environment | Config | Default |
|---|---|---|---|
| `--data` | `DATA` | `data` | (required) |
| `--views` | `VIEWS` | `views` | (required for `views`) |
| `--server` | `FHIR_BASE` | `server.base` | `http://localhost:3447/fhir` |
| `--server-name` | `SERVER_NAME` | `server.name` | `hapi-local` |
| `--token-url` | `FHIR_TOKEN_URL` | `server.token_url` | |
| | `FHIR_CLIENT_ID`, `FHIR_CLIENT_SECRET` | | no token |
| `--kiln` | `KILN` | `tools.kiln` | `kiln` on the PATH |
| `--countries` | | `tiles.countries` | every country |
| `--threads` | | | number of CPUs |
| `--reuse-raw` | `REFRESH_RAW=0` | | download again |
| `--full` | | | incremental |
| `--dry-run` | | | (push only) |

The client ID and secret are read only from the environment, never from flags
or the file. kiln uses them to get and renew its own tokens.

```toml
data = "../data"
views = "../ig/fsh-generated/resources"

[server]
base = "https://fhir.example.org/fhir"
name = "prod"
token_url = "https://auth.example.org/realms/x/protocol/openid-connect/token"

[tools]            # paths or names on the PATH
kiln = "kiln"
duckdb = "duckdb"
tippecanoe = "tippecanoe"
rclone = "rclone"

[tables]
strip_prefix = ["icr_"]

[partition]
column = "country"

[[partition.lookup]]        # a view with location_id: country from parquet/locations
key = "location_id"
table = "locations"
table_key = "id"

[[partition.lookup]]        # else campaign_id: from parquet/campaign_calendar
key = "campaign_id"
table = "campaign_calendar"

[tiles]
countries = ["NGA"]

[push]
remote = "r2"               # rclone remote
bucket = "warehouse"
dirs = ["parquet", "tiles", "views"]
files = ["manifest.json"]
exclude = [".DS_Store"]

[[hooks]]
name = "catalog"
after = "locations"         # only when that step ran; leave out to always run
run = ["./catalog.sh"]      # a path with / is relative to this file
```

Hooks run in the config file's directory with `DATA`, `FHIR_BASE`,
`SERVER_NAME` and `REFRESHED_AT` (the manifest's time) set.

Each run writes its step timings to `raw/<server>/last-refresh.json`.

## Development

```bash
cargo test        # unit tests, and end-to-end runs against tests/fixtures with a stub kiln
cargo clippy --all-targets -- -D warnings
```

## License

MIT, copyright Recode Labs. See [LICENSE](LICENSE).
