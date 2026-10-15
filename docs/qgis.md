# Editing fhir-pipes output in QGIS

The round trip from a fhir-pipes dataset to a FHIR server, step by step, using the
settlements file as the example. The same steps work for any partition file,
for the whole `locations/` directory, or for a subset you export yourself.

```
fhir-pipes transform locations ──▶ out/locations/.../type=settlement/part-0.parquet
                        │ open in QGIS, export a GeoJSON working copy
                        ▼
                   settlements-edits.geojson   (edit here)
                        │ fhir-pipes diff
                        ▼
                   changes.ndjson              (only what changed, as FHIR)
                        │ fhir-pipes load
                        ▼
                   FHIR server ──▶ fhir-pipes run ──▶ fresh dataset
```

## 1. Open the file

Layer > Add Layer > Add Vector Layer, and pick a partition file, for example
`out/locations/country=NG/geom_type=polygon/type=settlement/part-0.parquet`.
QGIS reads GeoParquet through GDAL, so any current QGIS build works.

The layer is in lon/lat (CRS84, shown by QGIS as EPSG:4326) and carries every
column: the writable ones such as `name`, `status`, `part_of`, `identifier`
and `position_longitude`/`position_latitude`; the derived ones such as
`admin1_name`, `path` and `tier`; and `fhir_json`, the complete resource.
See the section "Columns" in [locations.md](locations.md#columns) for which group each column is in.

To see everything at once, add the `locations/` directory itself as a layer.
The default layout is one file per country, geometry type and Location type,
so a facilities layer or a settlements layer needs no filter.

## 2. Make an editable copy

QGIS cannot write back into a Parquet file, so make a working copy: right
click the layer > Export > Save Features As. Choose **GeoJSON**, keep the CRS
at **EPSG:4326**, save it as something like `settlements-edits.geojson`, and
tick "Add saved file to map". This file is what you edit and what fhir-pipes diff
reads. GeoParquet is also accepted as the export format if you prefer it.

Exporting a selection only is fine: rows you do not export are left alone by
diff.

## 3. Edit

Toggle editing on the GeoJSON layer (the pencil icon) and work as usual.

- **Attributes.** Open the attribute table (F6). Change `name`, set `status`
  to `inactive` to retire a place, correct `part_of` to another Location's
  id, edit `pcode`, `settlement_type` and so on.
- **Geometry.** Move a point with the vertex tool, or redraw a polygon.
  Drawing a polygon on a place that only had a point adds a boundary to it.
- **New places.** Use the add-feature tool. Leave `id` empty and fhir-pipes assigns
  a UUID, or type the id you want. Fill in `name`, `type` and `part_of`.
- **Facilities are two resources.** Renaming a facility, retiring it, or
  changing its level or ownership updates both its Location and its
  Organization; the NHFR code, uid and the type labels live on the
  Organization only. A new facility row creates both.
- **Leave the derived columns alone.** `admin1_name`, `path`, `tier`,
  `depth`, `country` and the rest are rebuilt on the next transform and diff
  ignores them even if you change them.

Save the edits (Ctrl+S) and toggle editing off. The GeoJSON file on disk is
now the edits file.

Things that behave differently from a spreadsheet:

- **Blank means clear.** An empty `description` removes the description from
  the resource. Every column in the export is applied, so this is how a field
  gets cleared at all.
- **Deleting a feature does nothing.** fhir-pipes never deletes. Retire the place
  with `status = inactive`.
- **A point drawn on a place that has a boundary is ignored** and reported,
  since erasing a boundary that way is not a plausible intent. Other edits on
  the same row still apply.
- **Moving a point wins over the position columns.** The stale
  `position_longitude`/`position_latitude` values that came along in the
  export are not treated as an edit.
- **Float noise is not an edit.** Coordinates compare after rounding to seven
  decimals, so re-exporting an untouched layer produces no changes.

## 4. Diff

```sh
fhir-pipes diff --snapshot snapshot/ --in settlements-edits.geojson \
          --out changes.ndjson --report diff-report.json
```

diff reads the GeoJSON one feature at a time, finds each snapshot resource by
id, rebuilds it with only the writable columns applied, and writes the ones
that actually changed as complete FHIR resources, each carrying the version it
was based on. It prints a line such as `3 changed, 4212 unchanged, 1 new` and
a summary of anything it reported; `--report` writes the details as JSON.

Open `changes.ndjson` to see exactly what will be sent, or run a FHIR
validator over it. Nothing has touched the server yet.

## 5. Load

Dry run first, then for real:

```sh
fhir-pipes load --server https://fhir.example/fhir --in changes.ndjson --dry-run
fhir-pipes load --server https://fhir.example/fhir --in changes.ndjson
```

If the server needs authentication, pass `--token` (or set `FHIR_PIPES_TOKEN`), or
an OAuth client with `--client-id`, `--client-secret` and `--token-url` (or
`FHIR_PIPES_CLIENT_ID`, `FHIR_PIPES_CLIENT_SECRET`, `FHIR_PIPES_TOKEN_URL`); see
[Connecting to a FHIR server](../README.md#connecting-to-a-fhir-server). Load sends the resources parents first in transaction bundles, each
existing resource with a version check. If someone changed one of them on the
server since your snapshot was taken, the server refuses that bundle, fhir-pipes
names the conflicting ids, and nothing in that bundle is written. Refresh
(step 6) and redo that edit on the fresh copy.

## 6. Refresh

```sh
fhir-pipes run --server https://fhir.example/fhir --snapshot snapshot/ --out out/
```

pulls the new versions into the snapshot and rebuilds the dataset. Reopen the
Parquet layer in QGIS, delete the GeoJSON working copy, and you are back at
step 1.

## Working on a subset

Exporting only what you mean to edit keeps the working copy small. Select the
features in QGIS and tick "Save only selected features" in the export dialog,
or cut a subset with DuckDB:

```sql
COPY (SELECT * FROM read_parquet('out/locations/**/*.parquet', hive_partitioning = true)
      WHERE type = 'facility' AND admin1_name = 'Kano')
TO 'kano-facilities.parquet' (FORMAT PARQUET);
```

Either file goes through the same diff and load; the rows not in it are
untouched.
