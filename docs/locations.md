# Location registry to GeoParquet

The `locations` transform is the first of fhir-pipes' transforms: it turns
the `Location` resources in a FHIR server into a GeoParquet dataset that
QGIS, DuckDB, GeoPandas and web maps can read directly, and `diff` and `load`
carry edits made in those tools back into the FHIR server as ordinary,
version-checked FHIR updates. FHIR stays the master registry; the Parquet is
a derived, disposable projection of it. `index` and `population` add spatial
index identifiers and population Groups derived from the registry.

The command is `fhir-pipes transform locations`, and `fhir-pipes run` extracts
and transforms in one step. These commands came from kiln.

Most of this document describes the Integrated Campaign Registry (ICR), which
these commands were written for. ICR's extra columns, admin units, codes and
URLs come from a profile, not the code: run with
`--profile examples/profiles/icr.toml` (or ICR's own copy) to get the
behaviour described here. Without a profile only base FHIR is read. See
[profiles.md](profiles.md).

## Why the Location registry needs a GIS projection

Public health campaign systems built on FHIR keep their places, from the
country down to the village clinic, as `Location` resources. That is the right
home for them. A Location is linked to everything else in the registry: the
organisations that run it, the campaigns that target it, the people and
households recorded in it. It has identifiers from several systems, a version
history, and a defined profile.

It is also, for GIS purposes, close to unusable.

A FHIR server is a document store. Each Location is one JSON document, linked to
its parent by a `partOf` reference. To answer *how many clinics are in Kano
state* you follow `partOf` from every clinic up the chain, in application code,
one HTTP request at a time. Boundary polygons are base64 encoded GeoJSON inside
an attachment extension. There is no spatial index, no spatial query, no way to
ask for everything inside a bounding box. HAPI's Postgres tables are not a GIS
schema you can point PostGIS at, and hosted FHIR stores expose no SQL at all.

Meanwhile every GIS tool wants the same thing: a flat table with a geometry
column, ideally in a format it can read over HTTP without downloading all of it.
GeoParquet is that format. QGIS opens it. DuckDB queries it in place, from a
laptop or from a bucket. GeoPandas loads it. A web map can fetch only the row
groups that intersect the viewport.

fhir-pipes is the translation between those two worlds, in both directions.

### Why a projection and not a query layer

The alternative is to put a spatial API in front of the FHIR server. That means
a running service, a second copy of the data that has to stay in sync, and a
custom protocol every client has to learn. A file on disk or in a bucket is
simpler in every way: no server, nothing to keep alive, every GIS tool already
speaks it, and it can be rebuilt from the FHIR server at any time. If the
parquet is wrong, delete it and run fhir-pipes again. Nothing is lost.

### Why a single binary

fhir-pipes' users include people setting up a country deployment with limited
connectivity, on machines with a few gigabytes of memory, without a Python or
GDAL toolchain and without the ability to install one. The original Python
fhir-pipes needed Python 3.11, a package manager, and a system GDAL built against a
recent Arrow, because at the time GDAL was the only writer of the native
Parquet geometry type. That is no longer true. The Rust Parquet implementation
writes it directly, verified against GDAL, DuckDB and pyarrow (see
[docs/superpowers/spikes/2026-09-05-rust-native-geoparquet](superpowers/spikes/2026-09-05-rust-native-geoparquet)).
So fhir-pipes can be one file you copy onto a machine and run.

---

## What the GIS projection does

Given a FHIR server, fhir-pipes produces this:

```
FHIR server ──extract──▶ snapshot/ ──transform──▶ out/
                          (NDJSON)               (GeoParquet, partitioned)
```

and, when someone edits the data in a GIS tool, this:

```
edits.geojson ──diff (against snapshot)──▶ changes.ndjson ──load──▶ FHIR server
```

The transform is where the value is. Take a clinic as the FHIR server stores it:

```json
{
  "resourceType": "Location",
  "id": "clinic",
  "name": "Gama Clinic",
  "status": "active",
  "type": [{ "coding": [{ "code": "facility" }] }],
  "partOf": { "reference": "Location/gama" },
  "position": { "longitude": 3.25, "latitude": 6.25 },
  "identifier": [{ "system": "https://icr.example/identifiers/pcode", "value": "NG001002001" }]
}
```

Nothing in that record says which state or district the clinic is in. After
fhir-pipes, the same clinic is one row in a table alongside its neighbours:

```
id         name          type        admin_level  tier   admin0_name  admin1_name  admin2_name  geom_type
clinic     Gama Clinic   facility    -            site   Nigeria      Kano         Nassarawa    point
gama       Gama          settlement  -            site   Nigeria      Kano         Nassarawa    polygon
nassarawa  Nassarawa     admin-unit  2            2      Nigeria      Kano         Nassarawa    polygon
kano       Kano          admin-unit  1            1      Nigeria      Kano         -            polygon
ng         Nigeria       admin-unit  0            0      Nigeria      -            -            polygon
```

The clinic's row carries `Nigeria / Kano / Nassarawa`. fhir-pipes walked the
`partOf` chain for every record, skipped past `gama` because a settlement is
not an admin unit, and wrote the nearest real admin ancestor at each level as a
plain column. Every row also has a full ancestry path, a geometry, a
representative point, and the complete original FHIR resource as JSON, so
nothing is lost in the projection.

That table is what GIS tools want. Everything else in fhir-pipes exists to produce it
reliably, quickly, incrementally, and in a form that can be edited and pushed
back.

---

## The workflow

A typical cycle, for one country:

1. **Extract.** Pull Location resources from the FHIR server into a local
   snapshot. The first run fetches everything. Later runs fetch only what
   changed since the last run and merge it in.
2. **Transform.** Build the GeoParquet dataset from the snapshot. This is
   offline and fast, and can be re-run as often as you like.
3. **Publish.** Copy the output directory to wherever people read it: a shared
   drive, an object storage bucket, a static web server. This step is outside
   fhir-pipes; the output is plain files.
4. **Use.** Open in QGIS. Query with DuckDB. Join to campaign data. Draw a map.
5. **Edit.** Move a facility, correct a name, fix a ward boundary, add a new
   site. Do this in QGIS or any tool that can write GeoJSON or GeoParquet.
6. **Diff.** Compare the edited file to the snapshot. fhir-pipes works out which
   Locations actually changed and writes them as complete FHIR resources.
7. **Load.** Send the changed resources to the FHIR server, with a version
   check so an edit made on the server in the meantime is never overwritten.
8. **Extract again.** The snapshot picks up the new versions, and the cycle
   continues.

Steps 1 and 2 can run on a schedule or in response to a change notification.
Steps 5 to 7 happen when someone has something to fix.

---


## Commands

```
fhir-pipes extract   --server URL [AUTH] --snapshot DIR [--server-kind KIND]
               [--full] [--since INSTANT] [--timeout SECS]
               [--concurrency 8] [--retries 3] [--max-consecutive-failures 50]
               [--no-cache] [--refresh] [--cache-dir DIR]
fhir-pipes transform locations --snapshot DIR --out DIR [--country CC] [--row-group-size N] [--partition-by KEYS]
               [--keep-without-geometry]
fhir-pipes run       <extract flags> <transform flags>
fhir-pipes inspect   --out DIR
fhir-pipes diff      --snapshot DIR --in EDITS --out CHANGES.ndjson [--report FILE]
fhir-pipes load      --server URL [AUTH] --in CHANGES.ndjson
               [--dry-run] [--batch-size 100] [--concurrency 1] [--retries 3] [--timeout SECS]
fhir-pipes index     --snapshot DIR --spatial-index quadkey:18 [--spatial-index geohash:8]
               --out CHANGES.ndjson [--refresh] [--report FILE]
fhir-pipes import location-boundaries --in BOUNDARIES.geojson --country NAME=CODE --level … --out UNITS.ndjson
fhir-pipes import location-sites --in SITES.csv --boundaries UNITS.ndjson --type CODE … --out SITES.ndjson
fhir-pipes population --snapshot DIR --raster PATH_OR_URL --level N --year YYYY
               --out GROUPS.ndjson [--source worldpop] [--planning-denominator]
               [--estimate-date YYYY-MM-DD] [--report FILE] [--cache-dir DIR]
               [--retries 3] [--timeout 3600]
```

`AUTH`, timeouts and retries work as for every command; see
[Connecting to a FHIR server](../README.md#connecting-to-a-fhir-server).
`--server-kind` (`generic` by default; `medplum` or `aidbox`) adds the search
parameters those servers need to the Location and Organization searches, as
for `export`; see [servers.md](servers.md).

`extract` and `load` touch the network. `transform locations`, `inspect`, `diff`
and `import` never do. That split is deliberate: a slow server or an expired token has
nothing to do with whether your geometry is valid, and keeping them apart
means the whole geospatial half is testable from files with no server
involved.

## The snapshot

The snapshot is fhir-pipes' local copy of the registry and the unit of state
between runs.

```
snapshot/
  locations.ndjson       one Location resource per line, boundaries inlined
  organizations.ndjson   one Organization per line, for the facility pairing
  state.json             server URL, watermark, resource count, fhir-pipes version
  boundaries/            content addressed cache of fetched boundary attachments
  _extract_report.json   issues found by the last successful extract
  .incoming.ndjson       present only during, or after an interrupted, run
```

**`locations.ndjson`** holds every Location as the server returned it, with
one change: a boundary attachment that referred to a URL has been fetched and
inlined as base64 data, so the file is self contained. It is plain NDJSON. You
can `grep` it, `jq` it, diff two of them, or hand one to someone else.

**`state.json`** records the server URL and the watermark: the greatest
`meta.lastUpdated` across the whole snapshot. The next extract asks the server
for `Location?_lastUpdated=ge<watermark>` and merges the result by id: new
ids are appended, existing ids are replaced. The comparison is greater or
equal because servers differ in timestamp precision, so the resource that set
the watermark is refetched every run and replaces itself; the `updated` count
that extract prints therefore includes it. The merge rewrites the file, which
is a sequential pass and takes seconds even for a large country. Running an
incremental extract against a different server than the snapshot came from
is refused; `--full` repoints it. `--since INSTANT` overrides the stored
watermark for one run.

Organizations are paged after Locations with their own watermark,
`organization_watermark` in `state.json`, and merged the same way. A
snapshot written before fhir-pipes extracted Organizations has no file and no
watermark, so the next run fetches them in full while Locations stay
incremental.

Extract runs in three phases. It pages the search into `.incoming.ndjson`,
noting ids, timestamps and boundary URLs; it fetches the distinct boundary
URLs on `--concurrency` worker threads into the cache; and it merges the old
file and the incoming file into a new `locations.ndjson`, inlining boundary
bytes from the cache, then swaps it into place and writes `state.json`. A
failure in any phase leaves the previous snapshot untouched.

**`boundaries/`** is the boundary cache carried over from the Python fhir-pipes.
Admin boundaries change rarely, and a registry can have tens of thousands of
them, so every successfully fetched attachment is stored under a hash of its
URL and served from disk on later runs. A run that is killed halfway through a
long fetch resumes where it left off. Only successes are cached, so a
temporary outage does not become a permanent one. The cache is keyed by a
hash of the URL only, so `--cache-dir` can point several snapshots at one
shared directory; `--refresh` refetches everything but still writes the
cache, and `--no-cache` skips it entirely and holds fetched boundaries in
memory for the merge.

If `--max-consecutive-failures` boundary fetches in a row fail while nothing
in the run has succeeded, extract aborts rather than grinding through
thousands of attempts against a dead server or a bad token; `0` disables the
check. With the default timeout and retries a dead server can take over an
hour to trip it, because each attempt cycle runs the full timeout; lower
`--timeout` or the threshold for a fast failure. A scattering of dead URLs
in an otherwise healthy registry never trips it, because one success resets
the count.

A search response that is not a Bundle, for example an OperationOutcome or a
gateway's index page returned with status 200, aborts the run rather than
being read as an empty registry.

`_extract_report.json` has the same shape as transform's report and adds
these kinds: `page_resource_skipped` (a bundle entry with no resource object
or no id), `boundary_fetch_failed` (the attachment is left as a URL),
`boundary_stale_from_cache` (under `--refresh` the refetch failed and the
cached copy was used), `cache_error` (an unreadable or unwritable cache
entry, treated as a miss), `snapshot_line_unparsed` (a line in the old
snapshot with no id, copied unchanged), `organization_line_unparsed`
(the same for `organizations.ndjson`), and `count_mismatch` (the server's
resource count differs from the snapshot's after an incremental run).

### Incremental extract and its limits

Incremental extract makes the routine case fast: a registry that changed by a
few hundred resources since yesterday costs a few hundred resource fetches,
not a hundred thousand.

It has one blind spot. A resource that was **deleted** on the server does not
appear in any search result, so an incremental extract cannot see it go. Two
rules follow:

- Locations should be retired, not deleted. Set `status` to `inactive`. That
  is better registry practice anyway, because other resources may still refer
  to them.
- `fhir-pipes extract --full` discards the snapshot and fetches everything. Run it
  occasionally, or whenever you suspect drift.

To make drift visible, every incremental run ends by asking the server for
its resource count per type (`_summary=count`, or a zero-row search with
`_total=accurate` for servers such as the Google Healthcare API that reject
`_summary`) and comparing it with the snapshot. A difference is reported as `count_mismatch`, naming the two
numbers and pointing at `--full`; it cannot say which resource went, only
that one did. A server that returns no total is noted on stderr and the
check is skipped.

A full extract makes the same cross-check in one case: when its last page
held a full `_count` of entries and the server gave no next link. A result
set can end on an exactly full page, but a server that stops paging early
looks identical (HAPI FHIR 8.12 with its stock `search_prefetch_thresholds`
stopped after 3,000 of 51,843 Locations), and without the check a short
snapshot would pass as complete. The `count_mismatch` detail says which
case it is and that the snapshot is incomplete.

---

## The output dataset

`transform locations` writes one GeoParquet dataset per snapshot:

```
out/
  locations/
    country=NG/
      geom_type=polygon/
        type=admin-unit/part-0.parquet
        type=settlement/part-0.parquet
      geom_type=point/
        type=facility/part-0.parquet
        type=settlement/part-0.parquet
  _report.json
```

Partitioned by country, geometry type and Location type, so every kind of
place is its own file: open the facilities file in QGIS as one layer, or the
settlements file, without filtering, and the whole tree still reads as one
dataset in DuckDB or GDAL with the partition values as columns. Geometry type
stays in the split so a layer never mixes polygons and points, which QGIS
handles poorly; a type that occurs as both, such as settlements, gets one
file of each. `--partition-by` changes the split: `country,type` puts all
settlements in one file, `country,geom_type` is the two-file layout, and
`tier` splits admin units by level. A type with fewer than 100 rows is still
written, and reported as `small_partition`. Within each file, rows are
sorted along a Hilbert curve so that places near each other on the ground sit
near each other on disk, and every row group carries its bounding box in the
Parquet geospatial statistics. A reader asking for one district touches a few
kilobytes, including over HTTP with range requests.

The geometry column uses the native Parquet `GEOMETRY` logical type, with the
GeoParquet 1.1 file metadata and a `bbox` column alongside for readers that
predate the logical type. GDAL 3.12+, QGIS, DuckDB 1.5+, pyarrow 21+ and
GeoPandas all read it; older readers fall back to the metadata. The file
metadata's bounding box covers only the rows that have a geometry; a file
whose rows all lack one (only possible with `--keep-without-geometry`) has
no bounding box and an empty `geometry_types` list.

### transform locations options

| Option | Default | What it does |
|---|---|---|
| `--snapshot DIR` | required | The snapshot to read (`locations.ndjson`, and `organizations.ndjson` when present). |
| `--out DIR` | required | Where to write; the dataset goes to `DIR/locations` and the report to `DIR/_report.json`. |
| `--country CC` | from the level 0 admin unit | Puts every row under this country code instead of the one derived from the hierarchy. |
| `--partition-by KEYS` | `country,geom_type,type` | Comma-separated partition keys: any of `country`, `geom_type`, `tier`, `type`. |
| `--row-group-size N` | 20000 | Rows per Parquet row group. |
| `--keep-without-geometry` | off | Also writes Locations that have no point and no usable boundary, with the geometry columns null. Off, they are reported as `no_geometry` and left out. See [Columns](#columns). |

`fhir-pipes run` takes these options too (its `--snapshot` comes from the extract options).

### Columns

By default, one row per Location that has a usable geometry and a resolved
place in the hierarchy. A Location with no geometry, a boundary that did not
parse, an empty or unsupported geometry, a `partOf` chain that cycles or
exceeds twelve levels, or an id that repeats an earlier one is reported and
produces no row.

With `--keep-without-geometry`, every Location with a resolved place in the
hierarchy gets a row, whether or not it has geometry. This is for a table of
every place in the registry when most of them have no coordinates yet, such
as a DHIS2 org unit tree where only a few facilities carry GPS points. A
Location with no `position` and no usable boundary (none at all, a URL
boundary not yet fetched, or one that did not parse, was empty or was an
unsupported type) is written with `geometry`, `geom_type`, `lon`, `lat`,
`bbox`, `quadkey` and `quadkey_level` null. Every other column is filled as
usual: id, name, type, identifiers, pcode, the paired Organization, and the
whole derived hierarchy. Its issues still appear in the report, with
"written without geometry" added to their detail. A cycle, a chain deeper
than twelve levels, a repeated id or a line that does not parse still
produces no row. These rows sort after the rows with geometry in their file,
by country then id, and they count towards `duplicate_pcode`. With the
default partitioning they land in `geom_type=null/` directories, which
DuckDB reads as a null `geom_type`.

To add geometry later, edit the row: draw a point or a polygon on it in QGIS,
or set its geometry in DuckDB, then run `fhir-pipes diff` and `fhir-pipes load` (see
[Round trip](#round-trip-diff-and-load)). A point becomes the Location's
`position`; a polygon becomes its boundary. The next transform puts the row
in its point or polygon partition.

Without the flag, `geom_type`, `lon`, `lat` and `bbox` are required columns.
With it, those four are nullable in every file of the dataset, including
files whose rows all have geometry. `geometry` is nullable in both.

Columns are named after the FHIR path they come from
wherever one exists, so you can guess the column from the resource and the
resource from the column. They fall into four groups, and the group tells you
whether an edit to the column will flow back to FHIR.

**Identity and version. Read only.**

| column | from |
|---|---|
| `id` | `Location.id` |
| `version_id` | `Location.meta.versionId` |
| `last_updated` | `Location.meta.lastUpdated` |

**Writable FHIR content. Edits flow back through `diff`.**

| column | from |
|---|---|
| `name` | `Location.name` |
| `alias` | `Location.alias`, list of string |
| `status` | `Location.status` |
| `description` | `Location.description` |
| `type` | first `Location.type.coding.code` |
| `physical_type` | `Location.physicalType.coding.code` |
| `part_of` | `Location.partOf.reference`, as a bare id |
| `managing_organization` | `Location.managingOrganization.reference`, as a bare id |
| `identifier` | `Location.identifier`, list of struct `{system, value}` |
| `position_longitude`, `position_latitude` | `Location.position` |
| `geometry` | the boundary attachment extension, as a polygon; or the position, as a point |
| *the profile's Location columns* | ICR: `pcode` and `gers_id` from `identifier` by system (`pcode` from the pcode system, or failing that `national-admin-code`), `settlement_type` from the settlement-type extension's CodeableConcept, `facility_level` and `ownership` from `type` codings. See [profiles.md](profiles.md#column-kinds) |
| `quadkey`, `quadkey_level` | with a profile that names a spatial index extension: the deepest quadkey cell on the Location and its zoom; every prefix is the containing tile at a coarser zoom. Derived from `position` by fhir-pipes; read-only in the round trip |
| `organization_identifier` | the paired Organization's `identifier`, list of struct `{system, value}` |
| *the profile's Organization columns* | ICR: `nhfr_code` and `nhfr_uid` from the paired Organization's `identifier` by system, `facility_level_text` and `ownership_text` from the `text` of its `type` concepts holding those codings |

A facility in the ICR registry is a Location paired with an Organization
(`org-<location id>`) that carries the registry codes and the institution's
name and type. Rows whose Location names a managing organisation that is in
the snapshot carry its fields; the Location's own `facility_level` and
`ownership` codings are what the row shows for those two. A Location whose
Organization is missing is reported as `organization_missing`, and a name
that differs between the two as `organization_name_mismatch`.

A Location with both a boundary and a position is one row: the polygon is the
geometry, and the position is kept in its own columns.

**Derived hierarchy. Rebuilt on every transform, ignored by `diff`.**

| column | meaning |
|---|---|
| `depth` | number of `partOf` hops to the root |
| `admin_level` | 0 for country, 1 for the first subdivision, and so on; null for sites |
| `tier` | the admin level as text for admin units (`0` upward, in practice `0` to `4`), `site` for everything else |
| `path` | `/ng/kano/nassarawa/gama/clinic` |
| `ancestor_ids` | list of ids from the root down |
| `admin0_name` … `admin4_name`, `admin0_code` … `admin4_code` | nearest admin ancestor at each level (admin units and their codes come from the profile's `admin_unit_type` and `admin_code_column`); an admin unit deeper than level 4 is reported as `admin_level_beyond_columns` and does not appear in these columns |
| `overlays_admin_unit_ids` | with a profile that names an overlays extension: admin units this operational area overlaps, when it is not in the tree |
| `country` | the admin code of the level 0 ancestor (for ICR, the ISO code); `unknown` without a profile and without `--country` |
| `geom_type` | `polygon` or `point`; null for a row without geometry |
| `lon`, `lat` | the FHIR position when the resource has one, otherwise a point inside the geometry; a position that falls outside its own boundary is reported as `position_outside_boundary` and is written as given. Null for a row without geometry |
| `bbox` | struct of `xmin, ymin, xmax, ymax`; null for a row without geometry |

**Lossless fallback.**

| column | content |
|---|---|
| `fhir_json` | the complete Location resource as JSON text, with the boundary attachment removed |
| `organization_json` | the complete paired Organization as JSON text; null when there is none |

The last column is the guarantee that the projection loses nothing. Any field
fhir-pipes does not model as a column is still there, queryable with DuckDB's JSON
functions, and `diff` uses it as the base when reconstructing an edited
resource, so an extension fhir-pipes has never heard of survives a round trip
through QGIS untouched.

### The location_hierarchy table

`transform locations --hierarchy-out <dir>` also writes `location_hierarchy`,
one row per Location the walk resolved, with or without geometry. `refresh`
writes it to `parquet/location_hierarchy/` in its `locations` step. It is
partitioned by `country` (`country=<code>/part-0.parquet`, and
`country=__HIVE_DEFAULT_PARTITION__` for a Location with no level 0 code), and
`country` is also a column in the files. The built-in `LocationHierarchy`
Library reads it (see [libraries.md](libraries.md#locationhierarchy)).

| Column | Content |
|---|---|
| `id` | The Location's id |
| `name` | The Location's name |
| `part_of` | The id in the Location's `partOf` reference, not checked against the other Locations, so an orphan Location keeps the id of its missing parent |
| `is_admin` | Whether the Location is an administrative unit |
| `depth` | 0 at a root; a Location with no `partOf`, or whose parent is missing, is a root |
| `admin_level` | The Location's level among the administrative units on its chain; null for a Location that is not an administrative unit |
| `admin0_name` to `admin4_name` | The names of the first five administrative units on the chain, root first |
| `admin0_code` to `admin4_code` | Their codes |
| `path` | The ids from the root down to the Location, separated by `/`, with a leading `/` |
| `country` | `admin0_code`. It is the last column in the files |

### The report

`_report.json` records what transform found and could not or would not fix:
dangling parents, cycles, nodes orphaned by a cycle upstream, duplicate
p-codes, boundaries that did not parse, invalid polygons, facilities whose GPS
point falls outside their own district. fhir-pipes keeps going past all of them.
Deciding what to do about them is the registry owner's job, and the report is
how they find out.

The file has three keys. `counts` maps each issue kind to an exact count.
`issues` lists individual issues as `{kind, location_id, detail}`, capped at
1,000 per kind so a systematic fault cannot produce a report larger than the
dataset; `truncated` maps any capped kind to the number omitted. With
`--keep-without-geometry` there is a fourth, `written_without_geometry`: the
number of rows written with null geometry. Their `no_geometry` issues, and
any boundary issues on the same rows, are still counted, so with the flag
read those counts as "rows without geometry", not "rows dropped". The printed summary says the same on the line after
the row count. For a few
kinds `location_id` is not a single id: `duplicate_pcode` carries the
comma-joined ids, `small_partition` and the `partition_value_*` kinds carry
the partition path or `key=value`, and a resource with no id is `<unknown>`.

The kinds, grouped by where they arise:

- Resource shape: `missing_id`, `malformed_field`, `duplicate_id`
- Boundary attachment: `boundary_bad_content_type`, `boundary_bad_base64`,
  `boundary_empty`, `boundary_unresolved_url`, `boundary_unparseable`,
  `boundary_multi_feature`, `boundary_part_dropped`, `boundary_z_dropped`
- Geometry: `no_geometry`, `geometry_empty`, `geometry_unexpected_type`,
  `geometry_invalid`, `geometry_no_interior_point`, `position_outside_boundary`
- Hierarchy: `orphan`, `cycle`, `unreachable_ancestor`, `too_deep`,
  `no_country`, `admin_level_beyond_columns`
- Dataset: `duplicate_pcode`, `point_outside_parent`, `small_partition`,
  `partition_value_sanitized`, `partition_value_collision`, `row_vanished`
- Organization pairing: `organization_missing`, `organization_name_mismatch`
- Population (`fhir-pipes population`): `no_boundary`, `no_pixels`, `rollup_incomplete`,
  `rollup_empty`, `group_id_too_long`, `group_id_invalid`, `quantity_out_of_range`,
  `cache_error`, `unknown_source_code`

---

## How transform works: the two pass design

The Python fhir-pipes held everything in memory: every resource, every parsed
polygon, all at once in a dataframe. That is fine on a workstation and not
fine on a two gigabyte laptop with a country of detailed ward boundaries.

The Rust fhir-pipes reads the snapshot twice and never holds the geometries.

### Pass one: index

Read `locations.ndjson` line by line. For each resource, parse it and keep a
small record:

- the ids and text needed for the hierarchy: id, parent id, name, codes,
  type, status, the promoted extension values
- the **byte offset and length** of the line in the file
- the geometry's **kind and bounding box**, computed by parsing the boundary
  once and immediately discarding the parsed shape
- any diagnostics found while parsing

Then drop everything else. The record is on the order of a hundred bytes plus
its strings. A million Locations index into a few hundred megabytes at most,
and the polygons, which are where the real bulk is, are not resident.

All of fhir-pipes' logic runs on this index:

1. **Hierarchy.** Follow `partOf` from every node to the root, with
   memoisation so shared ancestor chains are walked once. Detect dangling
   parents, cycles, and nodes orphaned by a cycle. Assign depth, tier, path,
   ancestor list, and the nearest admin ancestor at each level, walking past
   settlements and operational areas that sit in the tree but are not admin
   units.
2. **Country.** From the level 0 ancestor, or the `--country` override.
3. **Data quality.** Duplicate p-codes, missing geometry, unresolved boundary
   URLs.
4. **Sort order.** A Hilbert curve key from the centre of each bounding box
   over a fixed world extent, so keys are the same across snapshots and are
   not disturbed by one far-flung outlier. Sort the index by partition, then
   by key. Rows kept by `--keep-without-geometry` have no box; they sort
   after all the others, by country then id.

### Pass two: write

Walk the sorted index. Keep one Parquet writer open per partition, created on
first use. For each record:

1. Seek to its byte offset and re-read the one line.
2. Parse the resource again, this time keeping the geometry.
3. Validate the polygon, compute the representative point, encode to WKB.
4. Build the row and append it to the partition's current batch.
5. Release the geometry.

Batches flush to a row group at the configured size. The one check that needs
two geometries at once, a point outside its parent's polygon, fetches the
parent by seeking to *its* offset; parents are few and checked repeatedly, so
a small cache of parent polygons is kept, bounded by count.

Peak memory is the index plus one batch per open partition plus the parent
cache. It does not grow with polygon size. The cost is reading the file twice,
and the second read is in sorted order rather than file order, so it is
seek-heavy. On an SSD this is invisible. On a spinning disk it is still far
cheaper than swapping.

### Atomic output

Every partition is written into a staging directory beside the live dataset.
Once all of them are complete the live dataset is renamed to a backup, the
staging directory is renamed into its place, and the backup is removed; a
later run that finds a leftover backup or staging directory cleans up before
it starts. A crash at any point
leaves either the old dataset or the new one, never a mixture and never a
truncated file that a reader might mistake for a complete one.

A failed transform leaves the previous dataset intact but removes
`_report.json`. A report describing a dataset it does not match is worse
than none: silence is at least honest about not knowing.

### Geometry handling

GeoJSON is parsed with the `geojson` crate into `geo` types. Validity is
checked and reported. It is **not** repaired: a self intersecting ward
boundary is a registry problem, and silently fixing it would hide that from
the people who need to know. The representative point is a `geo` interior
point. WKB is encoded directly. There is no GEOS and no GDAL anywhere in the
binary.

A Location with neither a `position` nor a usable boundary has no geometry.
By default it is reported and produces no row; with
`--keep-without-geometry` it is written with the geometry columns null, so
the dataset lists every place and geometry can be added later.

---

## Importing admin boundaries and sites

Two commands turn files from outside the registry into Location NDJSON for
`load`, to start a country's registry or add to it:

- `import location-boundaries` turns one level of admin boundaries (a GeoJSON
  file such as GRID3's operational wards) into the whole admin unit
  hierarchy above and including that level.
- `import location-sites` turns rows of points (a CSV of facilities, schools
  or settlements) into site Locations, each hung under the admin unit its
  row names, optionally each paired with a facility Organization.

They replace kiln's Python `bake` and `bake-points`, with the same options and
output. Both are offline: they read files and write NDJSON, and `load` sends
it. The resource shape comes from the profile (see
[What the profile provides](#what-the-profile-provides)); with ICR's profile
the output is what kiln wrote.

```
fhir-pipes import location-boundaries --in BOUNDARIES.geojson --country NAME=CODE
               --level LEVEL=NAME_PROP[:CODE_PROP] [--level …] [--alias LEVEL=PROPERTY …]
               [--code-system URI] [--dissolve-parents] --out UNITS.ndjson [--report FILE]
fhir-pipes import location-sites --in SITES.csv --boundaries UNITS.ndjson --type CODE
               --name-col COL --lat-col COL --lon-col COL --id-col COL
               --parent LEVEL=COLUMN [--parent …] [--identifier URI=COLUMN …] [--where COLUMN=VALUE …]
               [--paired-org] [--org-identifier URI=COLUMN …] [--org-type-coding URI=CODE_COL[:TEXT_COL] …]
               [--type-coding URI=CODE_COL[:TEXT_COL] …] [--spatial-index SCHEME:LEVEL …]
               --out SITES.ndjson [--report FILE]
```

A problem with the mapping is fatal: the command exits 2 and writes nothing,
because every resource would be wrong the same way. A problem with one
feature or row is reported and costs only that feature or row. The report
uses the same kinds as the other commands; `--report` saves it as JSON.

### import location-boundaries

The input is a GeoJSON FeatureCollection with one feature per unit of the
lowest level, each carrying the names (and optionally codes) of its
ancestors as properties. GRID3's ward files are like this:

```json
{"type": "Feature", "properties": {"state": "Bauchi", "statecode": "BA",
  "lga": "Alkaleri", "lga_alt_names": "", "ward": "Alkaleri East", …},
 "geometry": {"type": "Polygon", …}}
```

```sh
fhir-pipes import location-boundaries --profile examples/profiles/icr.toml \
  --in GRID3_NGA_operational_wards.geojson --country Nigeria=NGA \
  --level state=state:statecode --level lga=lga --level ward=ward \
  --alias lga=lga_alt_names --alias ward=ward_alt_names \
  --out units.ndjson
```

- `--country NAME=CODE` writes the root, level 0. Its id is the code as a
  slug (`nga`).
- Each `--level LEVEL=NAME_PROP[:CODE_PROP]` is one level below it, top
  down. The last is the level of the features and carries their boundaries.
  A unit above it is written once, the first time a feature names it.
- A unit's id is its parent's id, a hyphen, and the slug of its code, or of
  its name when the level has no code or the feature has none:
  `nga-ba-alkaleri-alkaleri-east`. A slug is lower case with accents folded
  to ASCII and each run of other characters made one hyphen
  (`N'Djaména` → `n-djamena`). So the ids are the same on every run, and
  units can be matched by name later.
- Each unit gets one identifier under `--code-system` (default: the
  profile's `[location] admin_code_system`, else `[dhis2]
  country_code_system`): its code, or its id when it has no code. The
  country's is the `--country` code.
- `--alias LEVEL=PROPERTY` reads `;`-separated alternative names into
  `alias`. `import location-sites` matches them too.
- The boundary is checked (a Polygon or MultiPolygon, rings with at least
  three distinct points and no self-intersection), rewound to RFC 7946
  (exterior rings counterclockwise, holes clockwise) and inlined as base64
  in the profile's boundary extension.
- `--dissolve-parents` also gives the country and every level above the last
  a boundary: the union of the last-level boundaries below it. It is derived,
  consistent with the units below it but not authoritative; a later official
  boundary for that unit can replace it.

Output is parents first: the country, then each level in the order its
units first appear. That is the order `load` needs. The features are read
one at a time; the last level's units wait in a temporary file next to
`--out` until the levels above are complete.

| Fatal (exit 2, nothing written) | Reported |
|---|---|
| Not a FeatureCollection | `missing_field`: a feature without a name at some level, skipped; or a name that slugs to nothing, which stops that feature at the level above |
| A `crs` other than WGS84 (reproject with `ogr2ogr -t_srs EPSG:4326`) | `geometry_invalid`: not a Polygon or MultiPolygon, or not valid; the unit is written without a boundary. With `--dissolve-parents`, a union that comes out empty |
| A `--level` property empty on every feature: a typo in the mapping | `invalid_id`: an id longer than 64 characters; the feature is skipped from that level down |
| Two different names with the same slug under one parent | `boundary_z_dropped`: 3D coordinates flattened to 2D |
| The same last-level unit twice | |

### import location-sites

The input is a CSV with a header row. Each row is one site, with its name, a
stable id, its coordinates and the names of the admin units it is in.

```sh
fhir-pipes import location-sites --profile examples/profiles/icr.toml \
  --in GRID3_NGA_health_facilities.csv --boundaries units.ndjson \
  --type facility --name-col facility_name --lat-col latitude --lon-col longitude \
  --id-col globalid --where state=Bauchi \
  --parent state=state --parent lga=lga --parent ward=ward \
  --identifier https://icr.healthcampaigns.org/identifiers/grid3-globalid=globalid \
  --paired-org \
  --org-identifier https://icr.healthcampaigns.org/identifiers/nga-nhfr-code=nhfr_facility_code \
  --org-type-coding https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs=facility_level:facility_level_option \
  --org-type-coding https://icr.healthcampaigns.org/CodeSystem/icr-ownership-cs=ownership:ownership_type \
  --type-coding https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs=facility_level:facility_level_option \
  --spatial-index quadkey:18 \
  --out facilities.ndjson
```

- Each row becomes a Location with the `--type` code under the profile's
  Location type system, physical type `si` (Site), the row's position, and
  an identifier for each `--identifier` whose cell is not empty. The
  `--id-col` value is the Location id, so it must be a valid FHIR id (up to
  64 letters, digits, `-` and `.`), such as GRID3's `globalid`.
- `--parent` columns name the row's admin units, top down. Each is matched
  against the names and aliases of the children of the unit matched above
  it, as slugs, so `Tama/Daye` matches `Tama Daye`. The first level is
  matched below the country when `--boundaries` has one root, as rows do not
  usually name the country. A row whose names stop matching part way is
  linked to the deepest unit that matched and reported as
  `parent_unresolved`; one that matches nothing has no `partOf`.
- `--where COLUMN=VALUE` keeps only rows whose cell is exactly the value.
- `--paired-org` writes a facility Organization before each Location, with
  id `<[organization] id_prefix><row id>` (`org-…`), and the Location names it
  as `managingOrganization`. Registry codes go on the Organization
  (`--org-identifier`); the Location keeps the place identifiers.
- `--org-type-coding URI=CODE_COL[:TEXT_COL]` adds a type to the
  Organization after `prov`: the cell as a slug is the code (`Primary` →
  `primary`), the cell itself the display, and the text column the concept
  text. `--type-coding` adds the same to the Location's type, after its type
  code, for tools that read only Locations.
- `--spatial-index SCHEME:LEVEL` writes the profile's spatial index
  extension on every row with a position, as `index` does.

| Fatal (exit 2, nothing written) | Reported |
|---|---|
| A column an option names is not in the CSV header | `missing_field`: no name; row skipped |
| Two rows with the same id | `invalid_id`: an id (or its Organization's id) that is not a valid FHIR id; row skipped |
| `--boundaries` has no line that is JSON | `missing_position`: coordinates missing, not numbers or out of range; written without a position |
| | `parent_unresolved`: see above |
| | `malformed_field`: a `--boundaries` line that is not JSON, skipped |

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

Physical types and the Organization `prov` type are base FHIR.

### Example: a new country

Sierra Leone from OCHA's common operational datasets: admin boundaries with
P-codes at three levels (province, district, chiefdom), and a health facility
list. The boundaries file has `ADM1_EN`, `ADM1_PCODE`, `ADM2_EN` … per
chiefdom. Using ICR's profile, with the P-code as each unit's code:

```sh
fhir-pipes import location-boundaries --profile examples/profiles/icr.toml \
  --in sle_admbnda_adm3.geojson --country "Sierra Leone=SLE" \
  --level province=ADM1_EN:ADM1_PCODE --level district=ADM2_EN:ADM2_PCODE \
  --level chiefdom=ADM3_EN:ADM3_PCODE \
  --code-system https://icr.healthcampaigns.org/identifiers/pcode \
  --dissolve-parents --out sle-units.ndjson --report sle-units-report.json
```

The chiefdom `SL040101` in district `SL0401` of province `SL04` gets the id
`sle-sl04-sl0401-sl040101` and an identifier with system
`https://icr.healthcampaigns.org/identifiers/pcode` and value `SL040101`, which the
`pcode` column of `transform locations` reads. Then the facilities, matched
by province, district and chiefdom name:

```sh
fhir-pipes import location-sites --profile examples/profiles/icr.toml \
  --in sle_health_facilities.csv --boundaries sle-units.ndjson \
  --type facility --name-col facility_name --lat-col lat --lon-col lon --id-col facility_id \
  --parent province=province --parent district=district --parent chiefdom=chiefdom \
  --paired-org --spatial-index quadkey:18 \
  --out sle-facilities.ndjson --report sle-facilities-report.json
```

Check the reports: `parent_unresolved` usually means a spelling differs
between the two files, which an alias in the boundaries file (`--alias`) or a
fix in the CSV resolves. Then load the units before the sites, so every
`partOf` exists when its child arrives:

```sh
fhir-pipes load --server https://fhir.example.org/fhir --in sle-units.ndjson --dry-run
fhir-pipes load --server https://fhir.example.org/fhir --in sle-units.ndjson
fhir-pipes load --server https://fhir.example.org/fhir --in sle-facilities.ndjson
```

For a country whose IG is not ICR's, write a profile first
([profiles.md](profiles.md)): `import` needs at least `[location]
type_systems` and `admin_unit_type`, and uses `[location] profile` and
`admin_code_system` when set.

### Differences from kiln's bake and bake-points

Checked by running both on the same inputs (the Python tests' cases, GRID3's
Bauchi wards with and without `--dissolve-parents`, and GRID3's facility
list) and comparing the NDJSON resource by resource: ids, names, aliases,
identifiers, `partOf`, types, extensions and report issues are the same, and
last-level boundaries are byte for byte the same. The differences:

- Systems, profiles and extension URLs come from the profile; with ICR's
  profile they are the ones kiln wrote.
- A dissolved boundary is computed with `geo` rather than GEOS. The polygons
  are the same to about 1e-9 of their area, but the vertices are in a
  different order.
- Validity is checked ring by ring (at least three distinct points, no
  self-intersection), as `transform locations` checks it. GEOS also checks
  how rings relate to each other, such as a hole outside its shell or
  overlapping parts of a MultiPolygon; such a boundary, which kiln reported
  as `geometry_invalid`, is written. The `geometry_invalid` detail text
  differs.
- 3D coordinates are flattened to 2D and reported as `boundary_z_dropped`;
  kiln kept them.
- An id longer than 64 characters is reported as `invalid_id` and skipped;
  kiln wrote it, and a FHIR server refuses it.
- `import location-sites` refuses a column the options name that the CSV
  lacks; kiln read every row's value as empty.
- `--org-identifier` and `--org-type-coding` need `--paired-org`; kiln
  ignored them without it.
- `--admin` is now `--boundaries`. Both commands take `--report`.

## Round trip: diff and load

Editing happens in GIS tools because that is where the good editing tools are.
QGIS can move a point, redraw a boundary, or edit a name in a table view, and
it can do it against the fhir-pipes parquet loaded as a layer. What it cannot do is
talk FHIR. `diff` and `load` close that gap.

### diff

```
fhir-pipes diff --snapshot snapshot/ --in edits.geojson --out changes.ndjson
```

Input is GeoJSON or GeoParquet, detected by extension, with an `id` column.
GeoJSON is what QGIS exports most naturally; GeoParquet suits DuckDB and
Python users.

The input is detected by extension: `.geojson` or `.json` for a
FeatureCollection, read one feature at a time so a whole-country export
never sits in memory; `.geojsonl` or `.geojsons` for one Feature per line;
`.parquet` for GeoParquet, read one record batch at a time, whatever
compression the exporting tool chose (Snappy, zstd, LZ4 or none). The row's id is
the `id` column, or the Feature id when the column is absent. A row with an
id the snapshot does not have becomes a new Location; a row with no id at
all is given a UUID and reported as `new_location_generated_id`.

For each row, diff finds the snapshot resource by id and rebuilds what the
resource *should* now be:

1. Start from the snapshot's complete resource.
2. Apply every **writable** column that is present in the input. A column
   that is absent leaves the field alone. A column that is present and
   empty removes the field: GIS exports carry every column, so an empty
   `description` has to mean "clear it" or a cleared field could never
   round trip. `identifier` is replaced as a whole list, then `pcode` and
   `gers_id` upsert their entry into it. A list column may arrive as a JSON
   string, which is how GDAL exports nested fields. On a facility row,
   `name`, `status`, `facility_level` and `ownership` apply to both the
   Location and its Organization when edited; the NHFR codes, the type
   labels and `organization_identifier` apply to the Organization only. A
   new `type = facility` row creates both, the Organization first.
3. Apply the geometry to what it came from. A row whose snapshot resource
   has a boundary gets its boundary attachment replaced when the polygon
   differs; a row without one gets its `position` moved when the point
   differs. Drawing a polygon on a point row adds a boundary. Drawing a
   point on a boundary row is reported as `geometry_kind_changed` and
   skipped, since erasing a boundary that way is not a plausible intent.
   A null geometry makes no geometry change. A row written by
   `transform locations --keep-without-geometry` has a null geometry, so it round
   trips unchanged; draw a point on it and the Location gains a `position`,
   draw a polygon and it gains a boundary.
   Comparison is on the WKB after rounding coordinates to seven decimals,
   so a float round trip through a GIS tool does not register as an edit,
   and coordinates are written back rounded the same way. Invalid polygons
   are reported as `geometry_invalid` and written as they are.
4. Ignore every **derived** column. A stale `admin1_name` in the input never
   causes a change. If the position columns and a point geometry were both
   edited and disagree, the geometry wins and `position_geometry_disagree`
   is reported. Position columns edited next to a point that was not moved
   keep their edit.
5. Compare the rebuilt resource to the original. If nothing changed, skip it.

What comes out is plain FHIR NDJSON containing only the resources that
changed, each complete, each carrying the `meta` it was based on, with a
new resource carrying none. You can inspect it, validate it against the
profile, or hand it to someone else before anything is sent. `--report`
writes the issues found as JSON in the same shape as transform's report;
the summary is always printed. The kinds are `duplicate_id`,
`new_location`, `new_location_generated_id`, `input_column_type` (a
writable column holding a value of the wrong type, ignored for that row),
`geometry_unparseable`, `geometry_kind_changed`, `geometry_invalid`,
`geometry_out_of_range` (a coordinate that is not a finite longitude and
latitude, so the geometry edit is skipped), `position_geometry_disagree`,
`boundary_z_dropped`,
`organization_missing` and `snapshot_line_unparsed`.

The input must be in WGS 84 longitude and latitude. A GeoParquet whose `geo`
metadata names another CRS, or a GeoJSON with a `crs` member that names one,
is refused before any row is read; reproject it first, for example with
`ogr2ogr -t_srs EPSG:4326`. A file that names no CRS is checked row by row:
a coordinate outside -180..180 and -90..90 is reported, in the geometry as
`geometry_out_of_range` and in the position columns as `input_column_type`,
and that row's position or geometry is left as it was.

### index

```
fhir-pipes index --snapshot DIR --spatial-index quadkey:18 --out changes.ndjson [--refresh] [--report FILE]
```

The ICR IG lets a point Location carry the **spatial index cell** it falls
in — the `spatial-index` extension: scheme (`quadkey`, `h3`, `geohash`),
level (quadkey zoom, H3 resolution, geohash precision) and the cell key —
so that de-duplication, tile-level joins and containment queries
(`Location?quadkey=0313131` is every point inside that tile, because FHIR
string search is prefix matching and quadkeys are prefix-hierarchical)
need no geometry engine. The cell is a pure function of the position, so
fhir-pipes owns it: `import location-sites --spatial-index quadkey:18` writes it at import,
and `index` backfills it onto Locations that were loaded without one.

`index` reads the snapshot, and for every Location that has a position but
lacks a cell at one of the requested scheme/level pairs, writes the resource
with the cell(s) added to the output NDJSON — `meta.versionId` and everything
else untouched, so `fhir-pipes load` sends it as the same version-checked `PUT` a
`diff` edit would be. Locations without a position and Locations that already
carry every requested cell are counted and skipped. `--refresh` recomputes
the requested cells even where present (after a bug, or a scheme change).
Re-running after a load and a fresh extract writes nothing.

`diff` keeps the cells honest on the way back: when an edit moves a
Location's position, every quadkey and geohash cell already on it is
recomputed for the new point (an H3 cell, which fhir-pipes cannot compute, is
dropped rather than left wrong), and clearing the position removes them all.
A cell of a scheme fhir-pipes does not know (S2, say) belongs to someone
else: it is left as it is and reported as `spatial_cell_not_recomputed`.
A cell whose level is outside its scheme's range (quadkey 1–23, geohash
1–12, H3 0–15) is reported as `malformed_field` and skipped: `transform`
leaves it out of the `quadkey` columns and `diff` does not recompute it.
Quadkey and geohash are computed in fhir-pipes with no dependency; H3 cells are
parsed and carried but not computed yet.

### population

```
fhir-pipes population --snapshot DIR --raster PATH_OR_URL --level N --year YYYY --out GROUPS.ndjson
                [--source worldpop] [--planning-denominator] [--estimate-date YYYY-MM-DD]
                [--report FILE] [--cache-dir DIR] [--retries 3] [--timeout 3600]
```

A campaign needs a denominator for every admin unit, and the ICR IG says
where it lives: not on the Location, but beside it as an
**`ICRTargetPopulation` Group** — a conceptual cohort (`actual = false`)
with a head count, scoped to the Location through its `geography`
characteristic, carrying provenance in extensions (`denominator-source`,
`denominator-type`, `estimate-date`, `is-calculated`,
`is-planning-denominator`). `population` produces those Groups from a
population raster and the boundaries already in the snapshot. It never
changes a Location.

`--raster` is a GeoTIFF: a local file, or an http(s) URL fetched once into
`SNAPSHOT/rasters/` (the same content-addressed cache the boundary fetch
uses; a response that is not a TIFF is refused before it can be cached).
WorldPop's constrained 100 m grids are the intended input — one band of
32-bit floats, EPSG:4326, `-99999` nodata, DEFLATE tiles — and any north-up
lon/lat single-band float GeoTIFF works. A projected raster, a PixelIsPoint
raster, or a multi-band file is refused with a message saying why. The
raster is held in memory (100–200 MB for a country). `--timeout` is the
deadline for one download attempt, one hour by default; as with `extract`,
each retry gets the full timeout again. The cache is keyed on the whole URL
and has no refresh flag: a raster republished at the same URL keeps being
served from `SNAPSHOT/rasters/` until that entry is deleted.

`--level N` picks the admin level to **measure**: every admin unit at that
level is summed against the raster under the *pixel-centroid rule* — a pixel
belongs to the unit whose polygon contains its centre. That rule makes
adjacent units a partition of the pixels, so children add up to their parent
exactly, where "all touched" methods count every shared-boundary pixel twice.
Each leaf count is rounded once; every admin ancestor of a measured unit then
gets a **rolled-up** total that is exactly the sum of its published
descendants, flagged `is-calculated = true`. Admin units at other levels are
not measured directly and the summary says how many; facilities and other
points are never counted. A unit the hierarchy pass dropped (a `partOf`
cycle, or a chain deeper than fhir-pipes follows) has no level and is not measured
either — it shows up only under pass one's own report kinds (`cycle`,
`too_deep`), so check those when the assigned share is lower than expected.

The last lines of output are the sanity check: the raster's grand total
against the sum assigned to the measured units. Expect close to 100%. Well
under means boundaries that do not cover the country, units missing at that
level, or the wrong raster — and a run in which *no* unit overlaps the raster
at all is an error, not an empty file.

Group ids are `pop-SOURCE-YEAR-LOCATIONID`, so a re-run rewrites the same
resources; `fhir-pipes load` sends them as plain `PUT`s (no `ifMatch` — fhir-pipes owns
these Groups outright). Load the Locations first: `load` orders `partOf`
parents before children but knows nothing about a Group's `geography`
reference. `--year` names the raster's reference year (it becomes part of
the id and the default `estimate-date` of `YYYY-01-01`); the IG defines
`estimate-date` as when the estimate was *made*, which a modelled raster does
not carry, so pass `--estimate-date` when you know it. `--source` is the
`denominator-source` code (`worldpop` by default; any code is written, a
code outside `icr-denominator-source-cs` is reported as
`unknown_source_code`). `--planning-denominator` marks the written Groups as
the figures campaigns plan against; the IG expects exactly one planning
denominator per geography, which fhir-pipes cannot check across runs.

Report kinds this command adds: `no_boundary` (a unit at `--level` had no
usable polygon), `no_pixels` (a polygon with no data-pixel centre inside —
smaller than a pixel, or outside the raster), `rollup_incomplete` (an
ancestor whose total under-counts because a descendant was `no_boundary`;
the Group is still written), `rollup_empty`, `group_id_too_long`,
`group_id_invalid`, `quantity_out_of_range`, `cache_error`,
`unknown_source_code`. The report also carries pass one's snapshot-quality
kinds (`no_geometry` for point-only facilities, `orphan`, `cycle`, …), since
`population` reads the snapshot with the same index `transform locations` builds.

What this is not: age–sex bands (WorldPop publishes those as separate
rasters; run `population` once per raster with a distinct `--source` when
the IG gains a characteristic for them), coverage-weighted sums, or two
competing estimates from different rasters for the same source and year —
their ids collide, and the second load replaces the first.


### load

```
fhir-pipes load --server URL [AUTH] --in changes.ndjson [--dry-run] [--batch-size 100]
          [--concurrency 1]
```

Load reads any NDJSON of FHIR resources with a `resourceType` and an `id`,
orders them parents first by `partOf` (matched by type and id, so a
Location and an Organization may share an id), groups them into transaction bundles
of `PUT <Type>/<id>`, and posts them with retry and backoff. Before the
first bundle it fetches the server's capability statement, which also
proves the URL and token work. If the input creates any new resource, the
server must support update-as-create for that type, because `PUT` to a new
id needs it. A server that advertises `updateCreate: false` for a needed
type is refused up front, since one clear error beats hundreds of identical
404s. A server that does not state the flag at all (the element is optional,
and HAPI FHIR omits it while supporting update-as-create) gets a warning and
the load proceeds; if it really cannot create on `PUT`, the first creating
bundle fails with the server's own error.

Every entry whose resource carries a `meta.versionId` is sent with
`ifMatch` set to that version. If the resource was changed on the server
after the snapshot was taken, the server answers 412 (some answer 409),
nothing in that bundle is written, and fhir-pipes reads each version-checked
resource in the bundle back to name exactly which ids conflicted and what
version the server has now. The run stops there; bundles before it stay
committed, and re-running the same file is safe because every entry is a
PUT by id with a version check. The fix is to extract again, re-apply the
edit, and diff again. This is what makes it safe for a GIS user to edit a
copy that might be a day old.

A bundle is sent again after a timeout or a 5xx. When the first attempt was
committed but its answer was lost, the retry conflicts with that attempt's
own writes. A 412 or 409 after more than one attempt says so: the error
gives the number of attempts and says the earlier one may have been
committed. Check the named resources on the server before extracting
again; if they hold the edit, the bundle was loaded.

Organizations are posted before Locations, so a new pair resolves within
the run.

`--concurrency N` (default 1) posts N bundles at a time, which suits large
sets of independent resources such as MeasureReports. No two bundles write
the same resource, as the input may not repeat an id. A bundle starts only
once every earlier bundle holding a resource it references (a `partOf`
parent, a `managingOrganization`, any other `reference` into the file) is
committed, so parents still land first on a server that checks
references; beyond that, bundles commit in any order, and the progress
lines come in the order they commit. The first failure stops new bundles
from starting; bundles already in flight finish, and those committed stay
committed.

`--dry-run` runs the preflight and prints one line per bundle, naming each
entry as `create` or `update@<version>`, without posting anything.

Load reads the input twice, so a large file does not have to fit in
memory. The first pass checks every line and keeps only each resource's
type, id, `meta.versionId`, line number and `partOf` parent; that is enough
to order the resources, check for cycles, run the preflight and print a dry
run. The second pass reads each bundle's resources again when a worker
takes the bundle, so only the bundles being posted are in memory. It reads
the file once per `partOf` depth, counted separately for Locations and for
other types: a MeasureReport file once, a Location hierarchy once per
level. The file must not change while it is loaded. Load checks its size
and modification time each time it opens it again, and checks that each
line still holds the resource planned for it, with the same type, id,
`meta.versionId` and `partOf.reference`. A change, or a file that no longer
reads, stops the run like a failed bundle (exit 1), and bundles committed
before it stay committed. With four workers and bundles
of 500, a 185 MB MeasureReport file loads in about 230 MB of memory instead
of 1.9 GB.

Load does not update the snapshot. The server is the authority on what was
stored; the next extract brings the snapshot up to date.

### What is writable

Only FHIR content is writable: the columns in the second group above,
including the profile's columns. `diff` refuses an edited file with a column
that is neither built in nor in the profile. The
hierarchy columns are computed and there is nothing to write them to. To move
a facility to a different district, change `part_of`. To rename a state,
change `name` on the state's row; every descendant's `admin1_name` will follow
on the next transform.

---

## Reading the output

The output is an ordinary GeoParquet dataset. Nothing about it is fhir-pipes
specific once written.

**DuckDB**, straight off the files, no extension needed for the geometry type:

```sql
SELECT admin1_name, count(*)
FROM 'out/locations/**/*.parquet'
WHERE type = 'facility'
GROUP BY admin1_name;
```

With the spatial extension, spatial predicates prune row groups using the
native statistics:

```sql
LOAD spatial;
SELECT id, name
FROM 'out/locations/**/*.parquet'
WHERE ST_Intersects(geometry, ST_MakeEnvelope(3, 6, 4, 7));
```

A field fhir-pipes did not promote to a column is still reachable:

```sql
SELECT id, fhir_json->>'$.extension[0].url' FROM 'out/locations/**/*.parquet';
```

**QGIS** opens the directory or a single partition file as a vector layer.
The edit-and-push-back loop, step by step, is in [docs/qgis.md](qgis.md).

**GeoPandas** reads it with `read_parquet`.

**Vector tiles.** fhir-pipes does not make tiles. Two paths that start from its
output:

- On demand: DuckDB's `ST_AsMVT` cuts a Mapbox Vector Tile per request from
  the parquet, behind a tiny tile server or in the browser with DuckDB WASM.
- Pre baked: tippecanoe over the partition files, one layer per Location
  type, produces a PMTiles archive for fully static hosting.

Both are a few lines of glue outside fhir-pipes, and both are rebuilt from the same
parquet after every transform.

For a production deployment the likely tile server is
[Martin](https://maplibre.org/martin/), a MapLibre project written in Rust.
It serves PMTiles from local files or over HTTP, so it can front the
tippecanoe output directly, and it has a GeoParquet source backed by DuckDB
that would cut tiles from fhir-pipes' parquet with no bake step at all. That
source is marked unstable at the time of writing; the PMTiles path is the
one to rely on until it settles.

---

## Design decisions

**FHIR is the master; parquet is a projection.** The parquet can always be
rebuilt. It is never the source of truth and never edited in place as the
record of what is true. This keeps FHIR's version history, references, and
profile validation as the single authority.

**Incremental extract, full transform.** Extract is incremental because
network is the slow part. Transform is always full because the alternative is
worse than it looks: renaming one state changes the derived columns of every
row beneath it, so partial parquet updates would have to track change
amplification through the hierarchy. A full transform from the snapshot is a
sequential pass that takes seconds to minutes and has no such state.

**Two passes over the snapshot instead of everything in memory.** Explained
above. The design targets machines with a few gigabytes, and polygons are
where the bytes are.

**Native Parquet geometry type plus legacy metadata.** The native type gives
readers per row group pruning. The GeoParquet 1.1 metadata and the `bbox`
column keep older readers working. Writing both costs a few bytes per row.

**Column names follow FHIR paths.** `status`, `part_of`,
`position_longitude`, `identifier`. Someone who knows the resource can find
the column, and someone who knows the column can find the field.

**`fhir_json` in every row.** The projection is lossless. Whatever fhir-pipes does
not model still survives, is still queryable, and still round trips.

**Report, don't repair.** Invalid geometry, duplicate codes, broken chains are
recorded and the run continues. Fixing them silently would hide registry
problems from the people whose job is to fix them.

**Version checked writes.** `If-Match` on every load is what makes editing a
stale copy safe. Without it, a GIS user working from yesterday's export could
overwrite today's server side correction without either party noticing.

**Generic FHIR.** Standard search, standard paging, standard transaction
bundles, standard `If-Match`. fhir-pipes has no dependency on any particular
server's extensions, so the same binary works against HAPI, a hosted FHIR
store, or anything else that implements R4.

**No GDAL, no GEOS, no Python at runtime.** The deployment constraint drives
this. Everything fhir-pipes needs from those libraries turned out to be available
in pure Rust, at the cost of not offering geometry repair, which fhir-pipes did not
want to offer anyway. The population raster is read the same way: the
pure-Rust `tiff` crate decodes WorldPop's DEFLATE tiles, and fhir-pipes' own
scanline fill does the zonal sum.

---

## What the locations commands do not do

- **Make tiles.** See above. Tippecanoe and DuckDB do this well from fhir-pipes'
  output.
- **Repair geometry.** Reported, never fixed.
- **Delete resources.** Retire them with `status = inactive`.
- **Merge countries.** One snapshot and one dataset per country. Separate
  outputs can be placed under one directory tree by hand, since the partition
  key includes the country.
- **Validate against the profile.** `diff` produces structurally correct
  resources; running them through a FHIR validator before load is the
  operator's call, and the NDJSON checkpoint exists so that is easy.

---

## Future extensions to the locations commands

Things that fit the design and are not in it yet, roughly in the order they
are likely to matter.

**Change notifications.** Today extract is run on a schedule or by hand. A
FHIR server that supports Subscriptions, or any outboard notification of
resource changes, could trigger `fhir-pipes run` so the parquet is never more than a
minute behind the registry. The watermark logic already supports this; only
the trigger is missing.

**Publishing.** A `--publish` target that copies the finished dataset to an
object storage bucket after the atomic swap, so the whole pipeline is one
command. Readers over HTTP would see either the old or the new dataset, never
a partial one.

**Multi country datasets.** Merge several snapshots into one dataset with
`country` as the top level partition, for a regional or global view. The
partition layout already accommodates it; what is missing is the merge
command and a decision about how the report combines.

**Tile baking as an optional step.** If a Rust tiler matures to the point of
matching tippecanoe's generalisation quality, `fhir-pipes tiles` could become an
optional subcommand. Until then, tippecanoe is the right tool and fhir-pipes stays
out of the way.

**Other resource types.** The same projection idea applies to any resource
with a spatial or hierarchical shape: `Organization` hierarchies, `Group`
households with a location, campaign `CarePlan` targets. The snapshot and
transform machinery is resource agnostic; the column mapping is not, and each
type would need its own. `Group` has a first foothold: `fhir-pipes population`
writes `ICRTargetPopulation` Groups, though it does not yet extract or
project them.

**Validation hook.** An optional call to a FHIR validator on `diff` output
before `load`, so profile violations are caught locally rather than by the
server.

**Geometry repair as an explicit opt in.** If a deployment wants fhir-pipes to fix
invalid polygons rather than report them, a `--repair` flag could do so. That
would mean taking on GEOS or a pure Rust equivalent, and it would need to be
clearly marked in the report which geometries were altered.

---


## Performance

A synthetic country can be generated for timing (one country, 20 states, 400
districts as 200-vertex polygons, and `FHIR_PIPES_BENCH_FACILITIES` facilities,
default 200,000):

```sh
cargo test --release --test generate_snapshot -- --ignored --nocapture
./target/release/fhir-pipes transform locations --snapshot target/bench/snapshot --out target/bench/out
```

Measured on an Apple Silicon laptop, single runs, release build:

| facilities | snapshot | wall time | peak memory |
|---|---|---|---|
| 200,000 | 60 MB | 2.0 s | 161 MB |
| 1,000,000 | 291 MB | 8.6 s | 568 MB |

The synthetic data is a best case: two partitions, thin records, and circular
polygons without holes. Real registries with detailed ward boundaries will be
slower per row, mostly in boundary parsing.

---
