# Editing fhir-pipes output in QGIS

The round trip from a dataset to a FHIR server: open a Parquet file in QGIS,
edit a GeoJSON copy, turn the edits into FHIR with `diff`, and send them with
`load`. The same steps work for any partition file, the whole `locations/`
directory, or a subset you export.

## 1. Open and export a working copy

Layer > Add Layer > Add Vector Layer, and pick a partition file such as
`out/locations/country=NG/geom_type=polygon/type=settlement/part-0.parquet`.
The layer is in lon/lat (EPSG:4326) and has every column; see
[Columns](locations.md#columns) for which are writable.

QGIS cannot write back to Parquet. Right click the layer > Export > Save
Features As, choose **GeoJSON** with CRS **EPSG:4326**, and save it, for
example as `settlements-edits.geojson`. Rows you do not export are left alone
by `diff`.

## 2. Edit

Toggle editing on the GeoJSON layer and work as usual: change attributes
(`name`, `status`, `part_of`, profile columns), move or redraw geometry, or add
features (leave `id` empty and fhir-pipes assigns a UUID). Do not edit derived
columns such as `admin1_name`, `path`, `tier` or `country`: `diff` ignores
them.

- A facility is two resources. Edits update both its Location and its
  Organization, and a new facility row creates both.
- A blank value clears the field.
- Deleting a feature does nothing: fhir-pipes never deletes. Set
  `status = inactive` to retire a place.
- A point drawn on a place that already has a boundary is ignored and
  reported.

## 3. Diff

```sh
fhir-pipes diff --snapshot snapshot/ --in settlements-edits.geojson \
          --out changes.ndjson --report diff-report.json
```

`diff` writes only the resources that changed, as complete FHIR resources
carrying the version they were based on. Nothing has touched the server yet.

## 4. Load

```sh
fhir-pipes load --server https://fhir.example/fhir --in changes.ndjson --dry-run
fhir-pipes load --server https://fhir.example/fhir --in changes.ndjson
```

For authentication, pass `--token` (or `FHIR_PIPES_TOKEN`) or an OAuth client;
see [Connecting to a FHIR server](../README.md#connecting-to-a-fhir-server).
Each existing resource is sent with a version check. If someone changed it on
the server since your snapshot, the server refuses that bundle, nothing in it
is written, and the conflicting ids are named. Refresh and redo that edit.

## 5. Refresh

```sh
fhir-pipes run --server https://fhir.example/fhir --snapshot snapshot/ --out out/
```

This pulls the new versions and rebuilds the dataset. Reopen the layer in QGIS.
To edit a subset only, select features and tick "Save only selected features"
when exporting.
