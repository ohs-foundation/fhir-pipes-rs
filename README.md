# kiln

**A bridge between a FHIR Location registry and modern GIS tools.**

kiln turns the `Location` resources in a FHIR server into a GeoParquet dataset
that QGIS, DuckDB, GeoPandas and web maps can read directly and fast, and
carries edits made in those tools back into the FHIR server as ordinary FHIR
updates. FHIR stays the master registry. The parquet is a derived, disposable
projection of it.

It ships as a single static binary with no runtime dependencies: no Python, no
GDAL, no database. It is designed to run on a laptop in a district office.

> **Status.** `extract`, `transform`, `run`, `inspect`, `diff` and `load` are
> implemented in Rust and are what this README describes. The Python package
> under `python/` still provides `bake` and `bake-points`; it is documented in
> [python/README.md](python/README.md). A snapshot written by the Python
> `extract` is accepted by the Rust binary: with no `state.json` it is treated
> as a full extract on the next run.

---

## Contents

- [Why kiln exists](#why-kiln-exists)
- [What kiln does](#what-kiln-does)
- [The workflow](#the-workflow)
- [Commands](#commands)
- [The snapshot](#the-snapshot)
- [The output dataset](#the-output-dataset)
- [How transform works: the two pass design](#how-transform-works-the-two-pass-design)
- [Round trip: diff and load](#round-trip-diff-and-load)
- [Reading the output](#reading-the-output)
- [Design decisions](#design-decisions)
- [What kiln does not do](#what-kiln-does-not-do)
- [Future extensions](#future-extensions)
- [Building](#building)
- [Repository layout](#repository-layout)

---

## Why kiln exists

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

kiln is the translation between those two worlds, in both directions.

### Why a projection and not a query layer

The alternative is to put a spatial API in front of the FHIR server. That means
a running service, a second copy of the data that has to stay in sync, and a
custom protocol every client has to learn. A file on disk or in a bucket is
simpler in every way: no server, nothing to keep alive, every GIS tool already
speaks it, and it can be rebuilt from the FHIR server at any time. If the
parquet is wrong, delete it and run kiln again. Nothing is lost.

### Why a single binary

kiln's users include people setting up a country deployment with limited
connectivity, on machines with a few gigabytes of memory, without a Python or
GDAL toolchain and without the ability to install one. The original Python
kiln needed Python 3.11, a package manager, and a system GDAL built against a
recent Arrow, because at the time GDAL was the only writer of the native
Parquet geometry type. That is no longer true. The Rust Parquet implementation
writes it directly, verified against GDAL, DuckDB and pyarrow (see
[docs/superpowers/spikes/2026-09-05-rust-native-geoparquet](docs/superpowers/spikes/2026-09-05-rust-native-geoparquet)).
So kiln can be one file you copy onto a machine and run.

---

## What kiln does

Given a FHIR server, kiln produces this:

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
kiln, the same clinic is one row in a table alongside its neighbours:

```
id         name          type        admin_level  tier   admin0_name  admin1_name  admin2_name  geom_type
clinic     Gama Clinic   facility    -            site   Nigeria      Kano         Nassarawa    point
gama       Gama          settlement  -            site   Nigeria      Kano         Nassarawa    polygon
nassarawa  Nassarawa     admin-unit  2            2      Nigeria      Kano         Nassarawa    polygon
kano       Kano          admin-unit  1            1      Nigeria      Kano         -            polygon
ng         Nigeria       admin-unit  0            0      Nigeria      -            -            polygon
```

The clinic's row carries `Nigeria / Kano / Nassarawa`. kiln walked the
`partOf` chain for every record, skipped past `gama` because a settlement is
not an admin unit, and wrote the nearest real admin ancestor at each level as a
plain column. Every row also has a full ancestry path, a geometry, a
representative point, and the complete original FHIR resource as JSON, so
nothing is lost in the projection.

That table is what GIS tools want. Everything else in kiln exists to produce it
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
   kiln; the output is plain files.
4. **Use.** Open in QGIS. Query with DuckDB. Join to campaign data. Draw a map.
5. **Edit.** Move a facility, correct a name, fix a ward boundary, add a new
   site. Do this in QGIS or any tool that can write GeoJSON or GeoParquet.
6. **Diff.** Compare the edited file to the snapshot. kiln works out which
   Locations actually changed and writes them as complete FHIR resources.
7. **Load.** Send the changed resources to the FHIR server, with a version
   check so an edit made on the server in the meantime is never overwritten.
8. **Extract again.** The snapshot picks up the new versions, and the cycle
   continues.

Steps 1 and 2 can run on a schedule or in response to a change notification.
Steps 5 to 7 happen when someone has something to fix.

---

## Commands

All commands are subcommands of one binary.

```
kiln extract   --server URL [AUTH] --snapshot DIR
               [--full] [--since INSTANT] [--timeout SECS]
               [--concurrency 8] [--retries 3] [--max-consecutive-failures 50]
               [--no-cache] [--refresh] [--cache-dir DIR]
kiln transform --snapshot DIR --out DIR [--country CC] [--row-group-size N] [--partition-by KEYS]
kiln run       <extract flags> <transform flags>
kiln inspect   --out DIR
kiln diff      --snapshot DIR --in EDITS --out CHANGES.ndjson [--report FILE]
kiln load      --server URL [AUTH] --in CHANGES.ndjson
               [--dry-run] [--batch-size 100] [--retries 3] [--timeout SECS]

AUTH:          --token T
             | --client-id ID --client-secret S --token-url URL
kiln index     --snapshot DIR --spatial-index quadkey:18 [--spatial-index geohash:8]
               --out CHANGES.ndjson [--refresh] [--report FILE]
kiln population --snapshot DIR --raster PATH_OR_URL --level N --year YYYY
               --out GROUPS.ndjson [--source worldpop] [--planning-denominator]
               [--estimate-date YYYY-MM-DD] [--report FILE] [--cache-dir DIR]
               [--retries 3] [--timeout 3600]
kiln dhis2 locations --dhis2 URL DHIS2_AUTH --instance NAME --out LOCATIONS.ndjson
               [--id-prefix P] [--country-code CC] [--facility-level N] [--root UID] [--report FILE]
kiln dhis2 measures  --dhis2 URL DHIS2_AUTH --instance NAME INDICATORS --out MEASURES.ndjson
kiln dhis2 reports   --dhis2 URL DHIS2_AUTH --instance NAME INDICATORS --locations FILE
               --period PE [--period PE …] --level N [--level N …] --out REPORTS.ndjson
               [--breakdown UID …] [--chunk 10] [--report FILE]

DHIS2_AUTH:    --dhis2-token d2pat_…  |  --dhis2-user U --dhis2-password P
INDICATORS:    any of --indicator-group UID, --indicator UID, --data-element-group UID,
               --data-element UID, --data-element-group-set UID, --data-set UID (repeatable)
```

A server that needs authentication gets one of two kinds:

- `--token` (or `$KILN_TOKEN`) is sent as a fixed bearer token. Getting it is
  up to you, and a run that outlasts it fails with a 401.
- `--client-id`, `--client-secret` and `--token-url` (or `$KILN_CLIENT_ID`,
  `$KILN_CLIENT_SECRET`, `$KILN_TOKEN_URL`) are an OAuth client, such as a
  Keycloak client with service accounts enabled. kiln trades them for a
  token with the client-credentials grant before the first request, and for
  a new one a minute before each token expires, so a long run does not
  outlive its token. A wrong secret or token URL fails before any FHIR
  request.

The two are mutually exclusive; with neither, requests carry no
Authorization header. Any FHIR R4 server that supports standard search
with `_lastUpdated`, `Bundle.link` paging and transaction bundles works; kiln
does not depend on any vendor's extensions.

```sh
export KILN_CLIENT_SECRET=...          # keep the secret out of shell history
kiln run --server https://gateway.example.org/fhir \
  --client-id key-me --token-url https://auth.example.org/realms/R/protocol/openid-connect/token \
  --snapshot snap/ --out out/
```

`--timeout` is the total deadline for one HTTP request, 300 seconds by
default; a large boundary over a slow link can legitimately take minutes.
Connection errors, 429 and 5xx responses are retried up to `--retries` times
with exponential backoff and jitter, honouring a numeric `Retry-After`. Any
other 4xx is final.

`extract`, `load` and `dhis2` touch the network. `transform`, `inspect` and
`diff` never do. That split is deliberate: a slow server or an expired token has
nothing to do with whether your geometry is valid, and keeping them apart means
the whole geospatial half is testable from files with no server involved.

---

## The snapshot

The snapshot is kiln's local copy of the registry and the unit of state
between runs.

```
snapshot/
  locations.ndjson       one Location resource per line, boundaries inlined
  organizations.ndjson   one Organization per line, for the facility pairing
  state.json             server URL, watermark, resource count, kiln version
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
snapshot written before kiln extracted Organizations has no file and no
watermark, so the next run fetches them in full while Locations stay
incremental.

Extract runs in three phases. It pages the search into `.incoming.ndjson`,
noting ids, timestamps and boundary URLs; it fetches the distinct boundary
URLs on `--concurrency` worker threads into the cache; and it merges the old
file and the incoming file into a new `locations.ndjson`, inlining boundary
bytes from the cache, then swaps it into place and writes `state.json`. A
failure in any phase leaves the previous snapshot untouched.

**`boundaries/`** is the boundary cache carried over from the Python kiln.
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
- `kiln extract --full` discards the snapshot and fetches everything. Run it
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

`transform` writes one GeoParquet dataset per snapshot:

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
GeoPandas all read it; older readers fall back to the metadata.

### Columns

One row per Location that has a usable geometry and a resolved place in the
hierarchy. A Location with no geometry, a boundary that did not parse, an
empty or unsupported geometry, a `partOf` chain that cycles or exceeds twelve
levels, or an id that repeats an earlier one is reported and produces no row.
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
| `pcode`, `gers_id` | promoted from `identifier` by system, for convenience; `pcode` comes from the ICR pcode system, or failing that the `national-admin-code` system |
| `settlement_type`, `delivery_strategy`, `facility_level`, `ownership` | the ICR profile extensions |
| `quadkey`, `quadkey_level` | the deepest quadkey spatial-index cell on the Location and its zoom; every prefix is the containing tile at a coarser zoom. Derived from `position` by kiln; read-only in the round trip |
| `nhfr_code`, `nhfr_uid` | promoted from the paired Organization's `identifier` by system |
| `organization_identifier` | the paired Organization's `identifier`, list of struct `{system, value}` |
| `facility_level_text`, `ownership_text` | the `text` of the paired Organization's `type` concepts holding those codings |

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
| `admin0_name` … `admin4_name`, `admin0_code` … `admin4_code` | nearest admin ancestor at each level; an admin unit deeper than level 4 is reported as `admin_level_beyond_columns` and does not appear in these columns |
| `overlays_admin_unit_ids` | admin units this operational area overlaps, when it is not in the tree |
| `country` | ISO code, from the level 0 ancestor |
| `geom_type` | `polygon` or `point` |
| `lon`, `lat` | the FHIR position when the resource has one, otherwise a point inside the geometry; a position that falls outside its own boundary is reported as `position_outside_boundary` and is written as given |
| `bbox` | struct of `xmin, ymin, xmax, ymax` |

**Lossless fallback.**

| column | content |
|---|---|
| `fhir_json` | the complete Location resource as JSON text, with the boundary attachment removed |
| `organization_json` | the complete paired Organization as JSON text; null when there is none |

The last column is the guarantee that the projection loses nothing. Any field
kiln does not model as a column is still there, queryable with DuckDB's JSON
functions, and `diff` uses it as the base when reconstructing an edited
resource, so an extension kiln has never heard of survives a round trip
through QGIS untouched.

### The report

`_report.json` records what transform found and could not or would not fix:
dangling parents, cycles, nodes orphaned by a cycle upstream, duplicate
p-codes, boundaries that did not parse, invalid polygons, facilities whose GPS
point falls outside their own district. kiln keeps going past all of them.
Deciding what to do about them is the registry owner's job, and the report is
how they find out.

The file has three keys. `counts` maps each issue kind to an exact count.
`issues` lists individual issues as `{kind, location_id, detail}`, capped at
1,000 per kind so a systematic fault cannot produce a report larger than the
dataset; `truncated` maps any capped kind to the number omitted. For a few
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
- Population (`kiln population`): `no_boundary`, `no_pixels`, `rollup_incomplete`,
  `rollup_empty`, `group_id_too_long`, `group_id_invalid`, `quantity_out_of_range`,
  `cache_error`, `unknown_source_code`

---

## How transform works: the two pass design

The Python kiln held everything in memory: every resource, every parsed
polygon, all at once in a dataframe. That is fine on a workstation and not
fine on a two gigabyte laptop with a country of detailed ward boundaries.

The Rust kiln reads the snapshot twice and never holds the geometries.

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

All of kiln's logic runs on this index:

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
   by key.

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

---

## Round trip: diff and load

Editing happens in GIS tools because that is where the good editing tools are.
QGIS can move a point, redraw a boundary, or edit a name in a table view, and
it can do it against the kiln parquet loaded as a layer. What it cannot do is
talk FHIR. `diff` and `load` close that gap.

### diff

```
kiln diff --snapshot snapshot/ --in edits.geojson --out changes.ndjson
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
   Comparison is on the WKB after rounding coordinates to seven decimals,
   so a float round trip through a GIS tool does not register as an edit,
   and coordinates are written back rounded the same way. Invalid polygons
   are reported as `geometry_invalid` and written as they are.
4. Ignore every **derived** column. A stale `admin1_name` in the input never
   causes a change. If the position columns and a point geometry were both
   edited and disagree, the geometry wins and `position_geometry_disagree`
   is reported.
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
`position_geometry_disagree`, `boundary_z_dropped`,
`organization_missing` and `snapshot_line_unparsed`.

### index

```
kiln index --snapshot DIR --spatial-index quadkey:18 --out changes.ndjson [--refresh] [--report FILE]
```

The ICR IG lets a point Location carry the **spatial index cell** it falls
in — the `spatial-index` extension: scheme (`quadkey`, `h3`, `geohash`),
level (quadkey zoom, H3 resolution, geohash precision) and the cell key —
so that de-duplication, tile-level joins and containment queries
(`Location?quadkey=0313131` is every point inside that tile, because FHIR
string search is prefix matching and quadkeys are prefix-hierarchical)
need no geometry engine. The cell is a pure function of the position, so
kiln owns it: `bake-points --spatial-index quadkey:18` writes it at import,
and `index` backfills it onto Locations that were loaded without one.

`index` reads the snapshot, and for every Location that has a position but
lacks a cell at one of the requested scheme/level pairs, writes the resource
with the cell(s) added to the output NDJSON — `meta.versionId` and everything
else untouched, so `kiln load` sends it as the same version-checked `PUT` a
`diff` edit would be. Locations without a position and Locations that already
carry every requested cell are counted and skipped. `--refresh` recomputes
the requested cells even where present (after a bug, or a scheme change).
Re-running after a load and a fresh extract writes nothing.

`diff` keeps the cells honest on the way back: when an edit moves a
Location's position, every quadkey and geohash cell already on it is
recomputed for the new point (an H3 cell, which kiln cannot compute, is
dropped rather than left wrong), and clearing the position removes them all.
Quadkey and geohash are computed in kiln with no dependency; H3 cells are
parsed and carried but not computed yet.

### population

```
kiln population --snapshot DIR --raster PATH_OR_URL --level N --year YYYY --out GROUPS.ndjson
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
cycle, or a chain deeper than kiln follows) has no level and is not measured
either — it shows up only under pass one's own report kinds (`cycle`,
`too_deep`), so check those when the assigned share is lower than expected.

The last lines of output are the sanity check: the raster's grand total
against the sum assigned to the measured units. Expect close to 100%. Well
under means boundaries that do not cover the country, units missing at that
level, or the wrong raster — and a run in which *no* unit overlaps the raster
at all is an error, not an empty file.

Group ids are `pop-SOURCE-YEAR-LOCATIONID`, so a re-run rewrites the same
resources; `kiln load` sends them as plain `PUT`s (no `ifMatch` — kiln owns
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
denominator per geography, which kiln cannot check across runs.

Report kinds this command adds: `no_boundary` (a unit at `--level` had no
usable polygon), `no_pixels` (a polygon with no data-pixel centre inside —
smaller than a pixel, or outside the raster), `rollup_incomplete` (an
ancestor whose total under-counts because a descendant was `no_boundary`;
the Group is still written), `rollup_empty`, `group_id_too_long`,
`group_id_invalid`, `quantity_out_of_range`, `cache_error`,
`unknown_source_code`. The report also carries pass one's snapshot-quality
kinds (`no_geometry` for point-only facilities, `orphan`, `cycle`, …), since
`population` reads the snapshot with the same index `transform` builds.

What this is not: age–sex bands (WorldPop publishes those as separate
rasters; run `population` once per raster with a distinct `--source` when
the IG gains a characteristic for them), coverage-weighted sums, or two
competing estimates from different rasters for the same source and year —
their ids collide, and the second load replaces the first.

### dhis2

```
kiln dhis2 locations --dhis2 URL --instance NAME --out LOCATIONS.ndjson [--id-prefix P] [--country-code CC]
kiln dhis2 measures  --dhis2 URL --instance NAME --indicator-group UID --out MEASURES.ndjson
kiln dhis2 reports   --dhis2 URL --instance NAME --indicator-group UID --locations LOCATIONS.ndjson
                     --period LAST_12_MONTHS --level 2 --level 3 --out REPORTS.ndjson
```

Reads a DHIS2 instance (Web API, 2.38 or later) and writes FHIR NDJSON for
`kiln load`. Nothing is written to DHIS2. Sign in with a personal access
token (`--dhis2-token`, `$KILN_DHIS2_TOKEN`, sent as `Authorization:
ApiToken …`) or a username and password (`--dhis2-user` / `--dhis2-password`,
`$KILN_DHIS2_USER` / `$KILN_DHIS2_PASSWORD`, Basic). `--dhis2` (or
`$KILN_DHIS2_URL`) is the instance's base URL; a trailing `/api` is fine.
Use a read-only DHIS2 user: a token can do whatever its user can.

`--instance` is a short name for the DHIS2 instance (letters, digits and
hyphens, up to 16 characters, e.g. `sl-demo`). It names the identifier
systems (`https://icr.healthcampaigns.org/identifiers/dhis2/NAME/orgunit`,
`…/indicator`), prefixes every Measure and MeasureReport id, and is the
`meta.tag` code (system `icr-project-tag-cs`) on everything written, so one
instance's resources can be found, or removed, as a set. Keep it the same
across runs: ids are deterministic, so a re-run replaces the same resources.

**locations** turns the org unit tree into ICR Locations. Levels above the
facility level (by default the deepest level) become admin units; the
facility level becomes facility Locations, each paired with an
`ICRFacilityOrganization` (`managingOrganization` → `org-<location id>`), as
kiln models facilities everywhere else. Location ids are `PREFIX-<uid>`
(`--id-prefix`, default the instance name). Each Location carries the org
unit UID as its `official` identifier, plus the DHIS2 code when there is
one. Polygon and MultiPolygon geometry becomes the boundary extension, a
Point becomes `position`. An admin unit DHIS2 has no geometry for (often
the country itself) gets the union of its children's polygons, reported as
`boundary_from_children`, so it still has a boundary and a row in the
dataset. A closed org unit is `inactive`. `--country-code`
puts a national admin code on the top org unit, which is what `transform`
partitions by (`SLE`, `NGA`, …). `--root UID` exports one subtree.

**measures** turns indicators into Measures, chosen by
`--indicator-group` and `--indicator` (both repeatable). Aggregate data
elements can be added with `--data-element-group` and `--data-element`:
each becomes a `cohort` Measure whose value is the data element's total
over all its disaggregations (`#{uid}`), identified under
`…/dhis2/NAME/data-element`; dashboards often show such counts ("BCG
doses given") beside the indicators. `--data-element-group-set` takes
every data element in every group of a group set (the demo's "Diagnosis"
set holds the morbidity counts). `--data-set` adds a data set's
reporting rate, DHIS2's `<uid>.REPORTING_RATE`: actual ÷ expected reports
× 100, a `proportion` Measure whose populations are the actual and
expected report counts, identified under `…/dhis2/NAME/data-set`. FHIR ids
cannot hold `_`, so its id is `NAME-<uid>.REPORTING-RATE`.

Each Measure's `topic` lists the item's groups, coded so a reader can
regroup items the way DHIS2 does: system
`https://icr.healthcampaigns.org/CodeSystem/dhis2/NAME/indicator-group`,
`…/data-element-group`, or `…/data-element-group-set/<set uid>` for a
data element group that belongs to a group set; the code is the group's
uid. A DHIS2 indicator
is numerator ÷ denominator × factor. A percent (factor 100) is scored
`proportion`, another factor `ratio`, and an indicator whose denominator is
`1` `cohort` with one population. The numerator and denominator
expressions and descriptions become the populations' criteria (language
`text/x-dhis2-expression`); the factor, indicator type, annualized flag
and decimals go in the `dhis2-indicator` extension; indicator groups
become `topic`. The id is `NAME-<uid>` and the canonical URL
`https://icr.healthcampaigns.org/Measure/NAME-<uid>`.

**reports** asks `/api/analytics` for the selected indicators at each
`--level` for each `--period`, and writes one `summary` MeasureReport per
indicator, org unit and period, id `NAME-<indicator>-<orgunit>-<period>`:

| DHIS2 | MeasureReport |
|---|---|
| indicator | `measure`, the Measure's canonical URL |
| org unit | `subject`, the Location that carries its UID, with its name as `display` |
| period | `period.start` / `period.end`, from DHIS2's own metadata; the period code is in the `dhis2-period` extension |
| numerator, denominator | `group.population` counts (`initial-population` for a cohort) |
| value ÷ factor | `group.measureScore`: 45.18 % is stored as 0.4518 |
| last analytics run | `date` |
| system name | `reporter.display` |

DHIS2 computes every value: it adds up the numerator and the denominator
for that org unit and period, then divides. kiln never sums or averages
indicator values; ask for each level you want. Requests use
`includeNumDen` and `skipRounding`, and send `Cache-Control: no-cache` so a
proxy cannot answer from before the last analytics run. Periods can be
ISO (`2025`, `2025Q1`, `202501`, `2025W5`, `2025April`, …) or relative
(`LAST_12_MONTHS`, `THIS_YEAR`, …); a period with no data returns no rows
and so no reports. Values reflect the last analytics table run, which kiln
prints at the start.

`--breakdown UID` (repeatable) also asks for every value broken down by a
DHIS2 dimension: a category such as Fixed/Outreach (`fMZEcRHuamy`), an org
unit group set such as Facility Type (`J5jldMd8OHv`), or a data element
group set. Each breakdown becomes a `group.stratifier` on the report
(code: the dimension, system `…/CodeSystem/dhis2/NAME/dimension`), with
one `stratum` per option (system `…/dimension/<uid>`) carrying its own
populations and score. DHIS2 computes each stratum the way it computes the
total, and leaves out items the dimension does not apply to (an indicator
whose denominator is a population figure has no Fixed/Outreach split);
those reports simply have no stratifier for it.

`--locations` is any NDJSON holding the Locations: the output of
`dhis2 locations`, or a snapshot's `locations.ndjson` once they are on the
server. A row whose org unit has no Location is skipped and reported once
as `unmapped_org_unit`. Requests carry `--chunk` indicators (10) at a
time; when DHIS2 answers that a result is over its row limit (E7128,
E7151) kiln splits the request in two and tries again.

Report kinds: `stratum_without_total` (a breakdown value whose total DHIS2
did not return), `fractional_count` (a numerator or denominator that is not a
whole number, rounded because FHIR counts are integers; common for
averages and annualised denominators), `bad_number`, `unmapped_org_unit`,
`period_without_dates`, `invalid_id`; for `locations`, `bad_geometry`,
`parent_not_exported`, `below_facility_level` and `boundary_from_children`.

The Sierra Leone demo instance is open, so the whole chain can be tried:

```sh
export KILN_DHIS2_URL=https://play.im.dhis2.org/stable-2-41-10
export KILN_DHIS2_USER=admin KILN_DHIS2_PASSWORD=district
kiln dhis2 locations --instance sl-demo --id-prefix sle --country-code SLE --out sl-locations.ndjson
kiln dhis2 measures --instance sl-demo --indicator-group srxUNt2w5jn --out sl-measures.ndjson
kiln dhis2 reports --instance sl-demo --indicator-group srxUNt2w5jn --locations sl-locations.ndjson \
  --period 2025 --period LAST_12_MONTHS --level 2 --level 3 --out sl-reports.ndjson
for f in sl-locations sl-measures sl-reports; do kiln load --server URL --in $f.ndjson; done
```

### load

```
kiln load --server URL [AUTH] --in changes.ndjson [--dry-run]
```

Load reads any NDJSON of FHIR resources with a `resourceType` and an `id`,
orders them parents first by `partOf`, groups them into transaction bundles
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
nothing in that bundle is written, and kiln reads each version-checked
resource in the bundle back to name exactly which ids conflicted and what
version the server has now. The run stops there; bundles before it stay
committed, and re-running the same file is safe because every entry is a
PUT by id with a version check. The fix is to extract again, re-apply the
edit, and diff again. This is what makes it safe for a GIS user to edit a
copy that might be a day old.

Organizations are posted before Locations, so a new pair resolves within
the run.

`--dry-run` runs the preflight and prints one line per bundle, naming each
entry as `create` or `update@<version>`, without posting anything.

Load does not update the snapshot. The server is the authority on what was
stored; the next extract brings the snapshot up to date.

### What is writable

Only FHIR content is writable: the columns in the second group above. The
hierarchy columns are computed and there is nothing to write them to. To move
a facility to a different district, change `part_of`. To rename a state,
change `name` on the state's row; every descendant's `admin1_name` will follow
on the next transform.

---

## Reading the output

The output is an ordinary GeoParquet dataset. Nothing about it is kiln
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

A field kiln did not promote to a column is still reachable:

```sql
SELECT id, fhir_json->>'$.extension[0].url' FROM 'out/locations/**/*.parquet';
```

**QGIS** opens the directory or a single partition file as a vector layer.
The edit-and-push-back loop, step by step, is in [docs/qgis.md](docs/qgis.md).

**GeoPandas** reads it with `read_parquet`.

**Vector tiles.** kiln does not make tiles. Two paths that start from its
output:

- On demand: DuckDB's `ST_AsMVT` cuts a Mapbox Vector Tile per request from
  the parquet, behind a tiny tile server or in the browser with DuckDB WASM.
- Pre baked: tippecanoe over the partition files, one layer per Location
  type, produces a PMTiles archive for fully static hosting.

Both are a few lines of glue outside kiln, and both are rebuilt from the same
parquet after every transform.

For a production deployment the likely tile server is
[Martin](https://maplibre.org/martin/), a MapLibre project written in Rust.
It serves PMTiles from local files or over HTTP, so it can front the
tippecanoe output directly, and it has a GeoParquet source backed by DuckDB
that would cut tiles from kiln's parquet with no bake step at all. That
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

**`fhir_json` in every row.** The projection is lossless. Whatever kiln does
not model still survives, is still queryable, and still round trips.

**Report, don't repair.** Invalid geometry, duplicate codes, broken chains are
recorded and the run continues. Fixing them silently would hide registry
problems from the people whose job is to fix them.

**Version checked writes.** `If-Match` on every load is what makes editing a
stale copy safe. Without it, a GIS user working from yesterday's export could
overwrite today's server side correction without either party noticing.

**Generic FHIR.** Standard search, standard paging, standard transaction
bundles, standard `If-Match`. kiln has no dependency on any particular
server's extensions, so the same binary works against HAPI, a hosted FHIR
store, or anything else that implements R4.

**No GDAL, no GEOS, no Python at runtime.** The deployment constraint drives
this. Everything kiln needs from those libraries turned out to be available
in pure Rust, at the cost of not offering geometry repair, which kiln did not
want to offer anyway. The population raster is read the same way: the
pure-Rust `tiff` crate decodes WorldPop's DEFLATE tiles, and kiln's own
scanline fill does the zonal sum.

---

## What kiln does not do

- **Make tiles.** See above. Tippecanoe and DuckDB do this well from kiln's
  output.
- **Repair geometry.** Reported, never fixed.
- **Delete resources.** Retire them with `status = inactive`.
- **Merge countries.** One snapshot and one dataset per country. Separate
  outputs can be placed under one directory tree by hand, since the partition
  key includes the country.
- **Import external data into Locations.** Turning a GRID3 ward file or a CSV
  of facilities into FHIR resources is the job of `bake` and `bake-points`,
  which remain in the Python package under `python/` for now. `kiln
  population` is the deliberate exception: it derives Groups from the
  registry's own boundaries and a published raster, and writes nothing onto
  a Location.
- **Validate against the profile.** `diff` produces structurally correct
  resources; running them through a FHIR validator before load is the
  operator's call, and the NDJSON checkpoint exists so that is easy.

---

## Future extensions

Things that fit the design and are not in it yet, roughly in the order they
are likely to matter.

**Change notifications.** Today extract is run on a schedule or by hand. A
FHIR server that supports Subscriptions, or any outboard notification of
resource changes, could trigger `kiln run` so the parquet is never more than a
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

**`bake` and `bake-points` in Rust.** Porting the import side of the Python
kiln, so the whole toolkit is one binary. The GeoJSON and CSV parsing are
straightforward; `--dissolve-parents` needs polygon union, which is available
in pure Rust but is the piece to prototype first.

**Tile baking as an optional step.** If a Rust tiler matures to the point of
matching tippecanoe's generalisation quality, `kiln tiles` could become an
optional subcommand. Until then, tippecanoe is the right tool and kiln stays
out of the way.

**Other resource types.** The same projection idea applies to any resource
with a spatial or hierarchical shape: `Organization` hierarchies, `Group`
households with a location, campaign `CarePlan` targets. The snapshot and
transform machinery is resource agnostic; the column mapping is not, and each
type would need its own. `Group` has a first foothold: `kiln population`
writes `ICRTargetPopulation` Groups, though it does not yet extract or
project them.

**Validation hook.** An optional call to a FHIR validator on `diff` output
before `load`, so profile violations are caught locally rather than by the
server.

**Geometry repair as an explicit opt in.** If a deployment wants kiln to fix
invalid polygons rather than report them, a `--repair` flag could do so. That
would mean taking on GEOS or a pure Rust equivalent, and it would need to be
clearly marked in the report which geometries were altered.

---

## Building

Requires a stable Rust toolchain.

```sh
cargo build --release
./target/release/kiln --help
```

Release binaries are built in CI for macOS (arm64, x86_64), Linux (x86_64 and
aarch64, statically linked against musl) and Windows (x86_64), as single
files with no runtime dependencies.

Tests:

```sh
cargo test
```

Integration tests read the fixture snapshot under `tests/fixtures/snapshot/`, run a
transform, and verify the output with DuckDB when it is on `PATH`; they skip
that check otherwise. DuckDB is never linked into kiln.

A synthetic country can be generated for timing (one country, 20 states, 400
districts as 200-vertex polygons, and `KILN_BENCH_FACILITIES` facilities,
default 200,000):

```sh
cargo test --release --test generate_snapshot -- --ignored --nocapture
./target/release/kiln transform --snapshot target/bench/snapshot --out target/bench/out
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

## Repository layout

```
kiln/
  Cargo.toml
  src/
    main.rs, cli.rs, error.rs
    fhir/        Location parsing, profile extensions, NDJSON reading
    index/       the pass one record, hierarchy, Hilbert key, partition choice
    geometry/    GeoJSON parse, validity, interior point, WKB
    write/       Parquet writers, geo metadata, atomic swap
    report.rs
    transform.rs, inspect.rs
    snapshot/    state.json, FHIR instants, merge by id with boundary inlining
    extract/     FHIR client with retry, paging, boundary fetch pool, boundary cache
    run.rs
    diff/        input rows from GeoJSON and GeoParquet, resource rebuild, canonical compare
    load/        parents-first ordering, transaction bundles with ifMatch, capability preflight
    cells.rs     kiln index: spatial-index backfill
    raster/      GeoTIFF reader (tiff crate), pixel-centroid zonal sum, raster fetch cache
    population.rs  kiln population: measure admin units, roll up, emit ICRTargetPopulation Groups
    dhis2/       kiln dhis2: DHIS2 Web API client, org units → Locations, indicators → Measures, analytics → MeasureReports
  tests/
    fixtures/snapshot/
    fixtures/raster/   a 40×30 COG with known sums
    transform.rs, extract.rs, diff.rs, load.rs, index.rs, population.rs, dhis2.rs
  python/        the Python package: bake and bake-points (extract and load are superseded by the Rust binary)
  docs/
    superpowers/ design specs, plans, and spikes
```
