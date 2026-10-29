# Location registry to GeoParquet

These commands turn the `Location` resources in a FHIR server into a GeoParquet dataset that QGIS, DuckDB, GeoPandas and web maps can read. `diff` and `load` carry edits made in those tools back to the server as version-checked FHIR updates. FHIR stays the master registry. The Parquet is a derived projection that can be rebuilt at any time. `import`, `index` and `population` create Locations and population Groups from files and rasters. The commands came from kiln.

Extra columns, admin units, codes and URLs (for example for the Integrated Campaign Registry, ICR) come from a profile, not the code. Run with `--profile examples/profiles/icr.toml` to get the ICR behaviour described here. Without a profile only base FHIR is read. See [profiles.md](profiles.md).

## The workflow

1. `extract`: pull Locations (and Organizations) into a local snapshot. Later runs fetch only what changed.
2. `transform locations`: build the GeoParquet dataset from the snapshot. Offline and repeatable.
3. Open and edit it in QGIS or any tool that writes GeoJSON or GeoParquet ([qgis.md](qgis.md)).
4. `diff`: compare the edited file with the snapshot and write only the changed Locations as complete FHIR resources.
5. `load`: send them to the server with a version check, so a newer edit on the server is never overwritten.
6. `extract` again to pick up the new versions.

## Commands

```
fhir-pipes extract   --server URL [AUTH] --snapshot DIR [--full] [--since INSTANT]
fhir-pipes transform locations --snapshot DIR --out DIR [--country CC] [--partition-by KEYS] [--keep-without-geometry]
fhir-pipes run       <extract flags> <transform flags>
fhir-pipes inspect   --out DIR
fhir-pipes diff      --snapshot DIR --in EDITS --out CHANGES.ndjson [--report FILE]
fhir-pipes load      --server URL [AUTH] --in CHANGES.ndjson [--dry-run] [--batch-size 100] [--concurrency 1]
fhir-pipes index     --snapshot DIR --spatial-index quadkey:18 --out CHANGES.ndjson
fhir-pipes import location-boundaries --in BOUNDARIES.geojson --country NAME=CODE --level … --out UNITS.ndjson
fhir-pipes import location-sites --in SITES.csv --boundaries UNITS.ndjson --type CODE … --out SITES.ndjson
fhir-pipes population --snapshot DIR --raster PATH_OR_URL --level N --year YYYY --out GROUPS.ndjson
```

`--help` on each command lists every flag. `AUTH`, timeouts and retries work as for every command; see [Connecting to a FHIR server](../README.md#connecting-to-a-fhir-server). `--server-kind` (`generic` by default; `medplum` or `aidbox`) adds the search parameters those servers need; see [servers.md](servers.md).

`extract`, `run` and `load` contact FHIR; `population` can download a raster. `transform locations`, `inspect`, `diff` and `import` work on files.

## extract and the snapshot

`extract` pages `Location` and `Organization` searches into a local snapshot, fetches any boundary attachment that is a URL and inlines it as base64, and replaces the snapshot files one at a time. A failure leaves the previous files, but Locations can be updated while Organizations and state are still old. A boundary that cannot be fetched may leave the URL or older cached geometry; check the extract report. Reaching `--max-consecutive-failures` stops the run.

```
snapshot/
  locations.ndjson       one Location per line, boundaries inlined
  organizations.ndjson   one Organization per line, for the facility pairing
  state.json             server URL, watermarks, resource count, fhir-pipes version
  boundaries/            cache of fetched boundary attachments
  _extract_report.json   issues found by the last extract
```

| Option | Default | What it does |
|---|---|---|
| `--snapshot DIR` | required | Where the snapshot lives. |
| `--full` | off | Discard the snapshot and fetch everything. |
| `--since INSTANT` | stored watermark | Override the watermark for one run. |
| `--concurrency N` | 8 | Worker threads for boundary fetches. |
| `--no-cache`, `--refresh`, `--cache-dir DIR` | | Skip, refetch, or relocate the boundary cache. Only successful fetches are cached. |
| `--max-consecutive-failures N` | 50 | Abort if N boundary fetches in a row fail with no success; `0` disables. |

Incremental runs ask the server for `Location?_lastUpdated=ge<watermark>` (the greatest `meta.lastUpdated`, capped at the server's search-start time minus five minutes when the response has a usable HTTP `Date`) and merge the result by id. An incremental extract against a different server than the snapshot came from is refused; `--full` repoints it.

**Deleted Locations are invisible to an incremental extract.** A search never returns them. Retire Locations by setting `status` to `inactive` instead of deleting them, and run `extract --full` now and then. Every incremental run also compares the server's resource count with the snapshot and reports `count_mismatch` if they differ. A full run does the same check when the server stops paging early, so a short snapshot is not mistaken for a complete one.

A search response that is not a Bundle (for example an OperationOutcome returned with status 200) aborts the run.

## transform locations

Builds one GeoParquet dataset from the snapshot, partitioned by country, geometry type and Location type, so each kind of place is its own file (one QGIS layer) and the whole tree still reads as one dataset in DuckDB or GDAL:

```
out/
  locations/country=NG/geom_type=polygon/type=admin-unit/part-0.parquet
                       geom_type=point/type=facility/part-0.parquet
  _report.json
```

Rows are sorted along a Hilbert curve and each row group carries its bounding box, so a reader asking for one district reads little data. The geometry column uses the native Parquet `GEOMETRY` type with GeoParquet 1.1 metadata and a `bbox` column for older readers (GDAL 3.12+, QGIS, DuckDB 1.5+, pyarrow 21+ and GeoPandas read it).

| Option | Default | What it does |
|---|---|---|
| `--snapshot DIR` | required | The snapshot to read. |
| `--out DIR` | required | Dataset goes to `DIR/locations`, report to `DIR/_report.json`. |
| `--country CC` | from the level 0 admin unit | Put every row under this country code. |
| `--partition-by KEYS` | `country,geom_type,type` | Any of `country`, `geom_type`, `tier`, `type`. |
| `--row-group-size N` | 20000 | Rows per row group. |
| `--keep-without-geometry` | off | Also write Locations with no point and no usable boundary, with null geometry columns. Otherwise they are reported as `no_geometry` and left out. |
| `--hierarchy-out DIR` | | Also write the `location_hierarchy` table. |

`run` takes the extract and transform options together. `inspect --out DIR` prints a summary of every Parquet file under `DIR` (rows, row groups, size, geometry types).

By default a Location produces a row only if it has a usable geometry and a resolved place in the hierarchy. A Location with no geometry, a `partOf` cycle or a chain deeper than twelve levels, or a repeated id is reported and produces no row. A Location with both a boundary and a position is one row: the polygon is the geometry and the position is kept in its own columns. An unparseable boundary is reported; a valid position is then used as point geometry.

To add geometry to a row written with `--keep-without-geometry`, draw a point or polygon on it, then `diff` and `load`. A point becomes the `position`; a polygon becomes the boundary.

### Columns

Columns are named after the FHIR path they come from. The group a column is in decides whether an edit flows back to FHIR.

**Identity and version. Read only:** `id`, `version_id` (`meta.versionId`), `last_updated` (`meta.lastUpdated`).

**Writable FHIR content. Edits flow back through `diff`:**

| column | from |
|---|---|
| `name`, `alias`, `status`, `description` | the same Location fields (`alias` is a list of strings) |
| `type` | first `Location.type.coding.code` |
| `physical_type` | `Location.physicalType.coding.code` |
| `part_of`, `managing_organization` | `partOf` and `managingOrganization` references, as bare ids |
| `identifier` | list of `{system, value}` |
| `position_longitude`, `position_latitude` | `Location.position` |
| `geometry` | the boundary attachment as a polygon, or the position as a point |
| profile columns | ICR: `pcode`, `gers_id`, `settlement_type`, `facility_level`, `ownership`, and from the paired Organization `nhfr_code`, `nhfr_uid`, `facility_level_text`, `ownership_text`. See [profiles.md](profiles.md#column-kinds) |
| `organization_identifier` | the paired Organization's `identifier` |

Only these columns are writable. `diff` refuses a file with a column that is neither built in nor in the profile. To move a facility to another district, change `part_of`. To rename a state, change `name` on its row; descendants' `admin1_name` follows on the next transform.

**Derived hierarchy. Rebuilt on every transform, ignored by `diff`:** `depth`, `admin_level`, `tier`, `path`, `ancestor_ids`, `admin0_name` to `admin4_name`, `admin0_code` to `admin4_code`, `country`, `geom_type` (`polygon` or `point`), `lon`, `lat`, `bbox`, `quadkey`, `quadkey_level` (with a profile that names a spatial index extension), and `overlays_admin_unit_ids` (with a profile that names an overlays extension). An admin unit deeper than level 4 is reported as `admin_level_beyond_columns`. With `--keep-without-geometry`, `geom_type`, `lon`, `lat` and `bbox` are nullable and rows with null geometry land in `geom_type=null/` directories.

**Lossless fallback:** `fhir_json` is the complete Location with the boundary attachment removed, and `organization_json` is the complete paired Organization (null if none). Any field not modelled as a column is still there, and `diff` keeps those fields from the snapshot, so an unknown extension survives a round trip through QGIS. The `identifier` and `organization_identifier` columns keep only system and value: leave them out of an edit unless you are replacing the identifiers.

In ICR a facility is a Location paired with an Organization (`org-<location id>`) that carries the registry codes. A missing Organization is reported as `organization_missing`, and a differing name as `organization_name_mismatch`.

### The location_hierarchy table

`transform locations --hierarchy-out <dir>` also writes `location_hierarchy`: one row per Location the hierarchy walk resolved, with or without geometry. `refresh` writes it to `parquet/location_hierarchy/` in its `locations` step. It is partitioned by `country` (`country=__HIVE_DEFAULT_PARTITION__` when there is no level 0 code). The built-in `LocationHierarchy` Library reads it (see [libraries.md](libraries.md#locationhierarchy)).

| Column | Content |
|---|---|
| `id`, `name` | The Location's id and name |
| `part_of` | The id in `partOf`, not checked against the other Locations, so an orphan keeps the id of its missing parent |
| `is_admin` | Whether the Location is an administrative unit |
| `depth` | 0 at a root; a Location with no `partOf`, or whose parent is missing, is a root |
| `admin_level` | Level among the administrative units on its chain; null if it is not one |
| `admin0_name` to `admin4_name`, `admin0_code` to `admin4_code` | The first five administrative units on the chain, root first |
| `path` | Ids from the root down, separated by `/`, with a leading `/` |
| `country` | `admin0_code`; the last column in the files |

### The report

`_report.json` records what transform found and could not fix: dangling parents, cycles, duplicate p-codes, unparseable boundaries, invalid polygons, facilities whose point falls outside their district. fhir-pipes keeps going past all of them; the registry owner decides what to do. The file has `counts` (per issue kind), `issues` (`{kind, location_id, detail}`, capped at 1,000 per kind) and `truncated` (how many were omitted). With `--keep-without-geometry` it also has `written_without_geometry`. The other commands write reports of the same shape.

## Reading the output

The output is an ordinary GeoParquet dataset.

```sql
SELECT admin1_name, count(*)
FROM 'out/locations/**/*.parquet'
WHERE type = 'facility'
GROUP BY admin1_name;
```

With `LOAD spatial;` DuckDB can also use spatial predicates such as `ST_Intersects`. QGIS opens the directory or a single partition file as a layer; the edit-and-push-back steps are in [qgis.md](qgis.md). GeoPandas reads it with `read_parquet`. These standalone Location commands do not make vector tiles (`refresh` can build PMTiles); tippecanoe or DuckDB `ST_AsMVT` can build them from the output.

## Importing admin boundaries and sites

Two commands turn files from outside the registry into Location NDJSON for `load`, to start a country's registry or add to it. They replace kiln's `bake` and `bake-points` with the same options and output. Both read files and write NDJSON; `load` sends it. A mapping problem is fatal (exit 2, nothing written); a problem with one feature or row is reported and costs only that feature or row. `--report` saves the report as JSON.

### import location-boundaries

Turns one level of admin boundaries (a GeoJSON FeatureCollection, one feature per lowest-level unit, with ancestor names as properties, such as GRID3 wards) into the whole admin hierarchy above and including that level.

```sh
fhir-pipes import location-boundaries --profile examples/profiles/icr.toml \
  --in GRID3_NGA_operational_wards.geojson --country Nigeria=NGA \
  --level state=state:statecode --level lga=lga --level ward=ward \
  --alias lga=lga_alt_names --out units.ndjson
```

- `--country NAME=CODE` writes the root (level 0); its id is the code as a slug (`nga`).
- `--level LEVEL=NAME_PROP[:CODE_PROP]`, top down. The last level carries the boundaries. A unit above it is written once.
- Unit ids are the parent's id, a hyphen, and the slug of the code (or name): `nga-ba-alkaleri-alkaleri-east`. They are the same on every run.
- `--code-system URI` sets the identifier system for each unit's code (default: the profile's `[location] admin_code_system`, else `[dhis2] country_code_system`).
- `--alias LEVEL=PROPERTY` reads `;`-separated alternative names into `alias`; `import location-sites` matches them too.
- `--dissolve-parents` gives every level above the last a boundary that is the union of the boundaries below it. It is derived, not authoritative.
- The input must be WGS84 (reproject with `ogr2ogr -t_srs EPSG:4326`). Boundaries are checked and rewound to RFC 7946.

Output is parents first, which is the order `load` needs.

### import location-sites

Turns a CSV of points (facilities, schools, settlements) into site Locations, each under the admin unit its row names, optionally each paired with a facility Organization.

```sh
fhir-pipes import location-sites --profile examples/profiles/icr.toml \
  --in GRID3_NGA_health_facilities.csv --boundaries units.ndjson \
  --type facility --name-col facility_name --lat-col latitude --lon-col longitude \
  --id-col globalid --parent state=state --parent lga=lga --parent ward=ward \
  --paired-org --spatial-index quadkey:18 --out facilities.ndjson
```

- `--id-col` is the Location id and must be a valid FHIR id (up to 64 letters, digits, `-` and `.`). Two rows with the same id are fatal.
- `--parent LEVEL=COLUMN` names the row's admin units, top down, matched against the names and aliases of the units in `--boundaries` as slugs. A row that stops matching part way is linked to the deepest match and reported as `parent_unresolved`.
- `--where COLUMN=VALUE` keeps only matching rows. `--identifier URI=COLUMN` adds identifiers.
- `--paired-org` writes a facility Organization (id `<[organization] id_prefix><row id>`) before each Location and sets `managingOrganization`. `--org-identifier` and `--org-type-coding` add to the Organization; `--type-coding` adds to the Location's type.
- `--spatial-index SCHEME:LEVEL` writes the spatial index extension, as `index` does.

### What the profile provides

| In the output | Profile key |
|---|---|
| Location type codes' system | `[location] type_systems` (the first), else `[dhis2] location_type_system` |
| Admin unit type code | `[location] admin_unit_type` |
| Location `meta.profile` | `[location] profile`, else `[dhis2] location_profile`; none: no `meta` |
| Organization `meta.profile` | `[organization] profile`, else `[dhis2] facility_organization_profile` |
| Admin code identifier system | `--code-system`, else `[location] admin_code_system`, else `[dhis2] country_code_system` |
| Boundary extension | `[location] boundary_extensions` (the first), else the HL7 one |
| Spatial index extension | `[location] spatial_index_extension` (needed for `--spatial-index`) |
| Organization id prefix | `[organization] id_prefix` |

For a country whose IG is not ICR's, write a profile first ([profiles.md](profiles.md)). `import` needs at least `[location] type_systems` and `admin_unit_type`.

### Example: a new country

Import the boundaries, import the facilities, check the reports, then load the units before the sites so every `partOf` exists when its child arrives:

```sh
fhir-pipes import location-boundaries --profile examples/profiles/icr.toml \
  --in sle_admbnda_adm3.geojson --country "Sierra Leone=SLE" \
  --level province=ADM1_EN:ADM1_PCODE --level district=ADM2_EN:ADM2_PCODE \
  --level chiefdom=ADM3_EN:ADM3_PCODE --dissolve-parents \
  --out sle-units.ndjson --report sle-units-report.json
fhir-pipes import location-sites --profile examples/profiles/icr.toml \
  --in sle_health_facilities.csv --boundaries sle-units.ndjson \
  --type facility --name-col facility_name --lat-col lat --lon-col lon --id-col facility_id \
  --parent province=province --parent district=district --parent chiefdom=chiefdom \
  --paired-org --out sle-facilities.ndjson
fhir-pipes load --server https://fhir.example.org/fhir --in sle-units.ndjson --dry-run
fhir-pipes load --server https://fhir.example.org/fhir --in sle-units.ndjson
fhir-pipes load --server https://fhir.example.org/fhir --in sle-facilities.ndjson
```

`parent_unresolved` usually means a spelling differs between the two files. Fix it with `--alias` or in the CSV.

## Round trip: diff and load

QGIS can move a point, redraw a boundary or edit a name, but it cannot talk FHIR. `diff` and `load` close that gap.

### diff

```
fhir-pipes diff --snapshot snapshot/ --in edits.geojson --out changes.ndjson
```

The input is detected by extension: `.geojson`/`.json` (FeatureCollection), `.geojsonl`/`.geojsons` (one Feature per line) or `.parquet` (GeoParquet). The row id is the `id` column, or the Feature id. A row with an id the snapshot does not have becomes a new Location; a row with no id gets a UUID and is reported as `new_location_generated_id`. The input must be WGS 84; a file that names another CRS is refused (reproject with `ogr2ogr -t_srs EPSG:4326`), and out-of-range coordinates are reported and ignored.

For each row, `diff` starts from the snapshot's complete resource and:

1. Applies every writable column present in the input. A column that is absent leaves the field alone. A column that is present and empty clears the field. `identifier` is replaced as a whole list.
2. Applies the geometry to what it came from: a polygon replaces the boundary, a point moves the `position`. Drawing a point on a boundary row is reported as `geometry_kind_changed` and skipped. A null geometry changes nothing. Coordinates are compared after rounding to seven decimals, so a float round trip does not count as an edit.
3. Ignores every derived column, so a stale `admin1_name` never causes a change. If a point geometry and the position columns were both edited and disagree, the geometry wins (`position_geometry_disagree`).
4. Skips the resource if nothing changed.

On a facility row, `name` and `status` apply to both the Location and its Organization, and the registry codes apply to the Organization. A new `type = facility` row creates both.

The output is plain FHIR NDJSON with only the changed resources, each complete and carrying the `meta` it was based on (a new resource carries none). Inspect or validate it before anything is sent. `--report` writes the issues as JSON.

### index

```
fhir-pipes index --snapshot DIR --spatial-index quadkey:18 --out changes.ndjson [--refresh] [--report FILE]
```

The ICR IG lets a point Location carry a spatial index cell (extension `spatial-index`: scheme `quadkey`, `h3` or `geohash`, level, and cell key), so de-duplication, tile joins and containment queries (`Location?quadkey=0313131`) need no geometry engine. `index` backfills the cell onto Locations that have a position but lack a cell at a requested scheme and level, and writes them to NDJSON with `meta.versionId` untouched, so `load` sends them as version-checked updates. Others are counted and skipped. `--refresh` recomputes cells even where present. Quadkey and geohash are computed by fhir-pipes; H3 cells are carried but not computed yet.

`diff` keeps cells correct on the way back: when an edit moves a position, existing quadkey and geohash cells are recomputed, and an H3 cell is dropped. Clearing the position removes them all.

### population

```
fhir-pipes population --snapshot DIR --raster PATH_OR_URL --level N --year YYYY --out GROUPS.ndjson
                [--source worldpop] [--planning-denominator] [--estimate-date YYYY-MM-DD]
```

Produces the `ICRTargetPopulation` Group for each admin unit: a head count scoped to the Location through its `geography` characteristic, with provenance in extensions. It reads a population raster and the boundaries in the snapshot, and never changes a Location.

| Option | Default | What it does |
|---|---|---|
| `--raster` | required | A single-band float GeoTIFF in EPSG:4326, local or an http(s) URL (fetched once into `SNAPSHOT/rasters/`). WorldPop constrained 100 m grids are the intended input. Projected, PixelIsPoint and multi-band files are refused. |
| `--level N` | required unless `--type` | The admin level to measure; ancestors get roll-ups. `--type CODE` instead measures matching Locations, with no roll-up. |
| `--year YYYY` | required | The raster's reference year. Part of the Group id and the default `estimate-date` (`YYYY-01-01`). |
| `--source` | `worldpop` | The `denominator-source` code. |
| `--planning-denominator` | off | Mark the Groups as the figures campaigns plan against. The IG expects one per geography; fhir-pipes cannot check this across runs. |
| `--estimate-date` | from `--year` | When the estimate was made, if you know it. |

Each pixel belongs to the unit whose polygon contains its centre, so adjacent units never count a pixel twice. Units at `--level` are measured. Every admin ancestor gets a rolled-up total equal to the sum of its published descendants, flagged `is-calculated = true`. The last lines of output compare the raster's grand total with the sum assigned to the units; expect close to 100%. A run where no unit overlaps the raster is an error.

Group ids are `pop-SOURCE-YEAR-LOCATIONID`, so a re-run rewrites the same resources. `load` sends them as plain `PUT`s without `ifMatch`. Load the Locations first. Two rasters with the same source and year collide: the second load replaces the first. The raster cache is keyed on the URL and has no refresh flag; delete the cache entry to refetch.

### load

```
fhir-pipes load --server URL [AUTH] --in changes.ndjson [--dry-run] [--batch-size 100] [--concurrency 1]
```

Sends any NDJSON of FHIR resources that have a `resourceType` and an `id`. It orders them parents first by `partOf`, groups them into transaction bundles of `PUT <Type>/<id>`, and posts them with retry and backoff. Organizations are posted before Locations. Before the first bundle it fetches the capability statement, which also proves the URL and token work.

Safety rules:

- **Version check.** Every entry whose resource has a `meta.versionId` is sent with `ifMatch` set to that version. If the resource changed on the server after the snapshot was taken, the server answers 412 (some answer 409), nothing in that bundle is written, and fhir-pipes names the conflicting ids and the server's current versions. The run stops; bundles before it stay committed. To fix a conflict: extract again, re-apply the edit, and diff again.
- **Retries.** A bundle is sent again after a timeout or a 5xx. If the first attempt was committed but its answer was lost, the retry can conflict with its own writes. A 412 or 409 after more than one attempt says so; check the named resources on the server before extracting again.
- **Dry run.** `--dry-run` runs the preflight and prints one line per bundle, naming each entry as `create` or `update@<version>`, without posting anything. Use it first.
- **Creating resources.** New resources need update-as-create (`PUT` to a new id). A server that advertises `updateCreate: false` for a needed type is refused up front. A server that does not state it gets a warning and the load proceeds. Keep `meta.versionId` on resources you update: without it the `PUT` is unconditional and can overwrite an existing resource with the same id.
- **`--concurrency N`** (default 1) posts N bundles at a time. A bundle starts only after every earlier bundle holding a resource it references is committed. The first failure stops new bundles; those already committed stay committed.
- **The input file must not change while it is loaded.** Load reads it more than once and stops with exit 1 if it changes.

`load` does not update the snapshot. The next `extract` does.

## What the locations commands do not do

- No deletion. Retire Locations with `status: inactive`.
- Derived columns are not written back.
- These standalone Location commands do not build tiles or publish; `refresh` can do both.
