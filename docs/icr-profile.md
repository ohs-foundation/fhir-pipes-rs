# The ICR profile

The Integrated Campaign Registry (ICR) is a FHIR implementation guide (IG) for
health campaigns. Its fhir-pipes profile lives with the IG, at
[`ig/fhir-pipes-profile.toml`](https://github.com/recodelabs/icr/blob/main/ig/fhir-pipes-profile.toml)
in recodelabs/icr. [examples/profiles/icr.toml](../examples/profiles/icr.toml)
is a copy. This page shows what it maps. The file lists every key; the key
reference is [profiles.md](profiles.md).

## The example data

[examples/icr/snapshot](../examples/icr/snapshot) holds six Locations and one
Organization from ICR's server: one branch of the Nigerian admin tree (Nigeria,
Bauchi, Toro), a clinic with its catchment, and a settlement. The boundaries
are cut down to bounding boxes, and the settlement has a `settlement-type`
extension that the IG defines but ICR's data does not yet carry.

Transform keeps a copy of the profile it used beside the snapshot, so work on
a copy:

```sh
cp -r examples/icr/snapshot /tmp/snap
fhir-pipes transform locations --profile examples/profiles/icr.toml \
  --snapshot /tmp/snap --out /tmp/icr
```

Without the profile every row is `country=unknown` and the admin columns are
empty. With it, rows sit under `country=NGA` and these columns are added:

- after `position_latitude`: `pcode`, `gers_id`, `settlement_type`,
  `facility_level`, `ownership`, `quadkey`, `quadkey_level`
- after `organization_identifier`: `nhfr_code`, `nhfr_uid`,
  `facility_level_text`, `ownership_text`

## What it maps

| Profile entry | Reads | ICR IG definition |
|---|---|---|
| `admin_unit_type = "admin-unit"` | Admin levels from `partOf` through admin units: Nigeria is level 0, Bauchi 1, Toro 2 | CodeSystem `icr-location-type-cs` |
| `pcode` (`identifier`, two systems tried in order) | The admin code; `admin_code_column = "pcode"` fills `adminN_code` and gives `country` (Nigeria's `NGA`). Duplicates are reported as `duplicate_pcode` | `…/identifiers/pcode`, `…/identifiers/national-admin-code` |
| `gers_id` | Overture GERS id (empty in this data) | `…/identifiers/overture-gers` |
| `facility_level`, `ownership` (`type_coding`, `mirror_to_organization = true`) | The code in `type` under the system | CodeSystems `icr-facility-type-cs`, `icr-ownership-cs` |
| `facility_level_text`, `ownership_text` (`type_text`, Organization) | The `text` of that concept | same |
| `nhfr_code`, `nhfr_uid` (Organization `identifier`) | National health facility registry ids | `…/identifiers/nga-nhfr-code`, `…/identifiers/nga-nhfr-uid` |
| `settlement_type` (`extension_code` with `system`) | The code in the extension's `valueCodeableConcept` | Extension `settlement-type` |
| `spatial_index_extension` | `quadkey`, `quadkey_level`; lets `index` run and `diff` recompute cells after a position edit | Extension `spatial-index` |
| `boundary_extensions` | Where `diff` writes a new boundary and which extension `extract` inlines from. The URL ends in `/StructureDefinition/location-boundary-geojson`, so it is read without the profile too | Extension `location-boundary-geojson` |
| `catchment_extension` | `catchment_of` in the catchment tiles `refresh` builds (not a column) | Extension `catchment-of` |
| `[organization]` `facility_type = "facility"`, `id_prefix = "org-"` | `diff` gives a new facility Location an Organization with id `org-<location id>` | |
| `type_systems` | `population --type facility-catchment` matches the code under the current or the older location-type URI | |
| `[population]`, `[dhis2]` | URLs and extensions on the Groups, Locations, Organizations and MeasureReports those commands write | The ICR profiles, e.g. `ICRTargetPopulation`, `ICRLocation`, `ICRFacilityOrganization` |

Toro's admin code comes from `…/national-admin-code`, since it has no
`…/pcode` identifier. Rows below an admin unit take the names and codes of the
units above them:

| name | type | admin_level | admin1_name | admin2_name | admin2_code | country |
|---|---|---|---|---|---|---|
| Nigeria | admin-unit | 0 | | | | NGA |
| Bauchi | admin-unit | 1 | Bauchi | | | NGA |
| Toro | admin-unit | 2 | Bauchi | Toro | 5018 | NGA |
| Makera Primary Health Clinic | facility | | Bauchi | Toro | 5018 | NGA |

Anything the profile does not name stays unchanged in `fhir_json`, and
identifiers also appear in the `identifier` list column. For example, the
settlement's GRID3 identifiers, the catchment's `catchment-of` reference and
the `building-count` extensions are in `fhir_json` only.

## Writing an edit back with diff

An edit as GeoJSON, with only the changed properties:

```json
{"type": "FeatureCollection", "features": [
  {"type": "Feature", "geometry": null,
   "properties": {"id": "e164b9cb-92d5-4c2b-a415-46c50945370c", "settlement_type": "hard-to-reach"}},
  {"type": "Feature", "geometry": null,
   "properties": {"id": "2d14d838-685c-4723-9f73-06392f7ff353", "facility_level": "secondary"}}
]}
```

```sh
fhir-pipes diff --snapshot /tmp/snap --in edits.geojson --out changes.ndjson
```

`diff` takes the profile from the copy in `/tmp/snap` and writes each edit
where the column reads it from:

- The settlement's `settlement-type` extension is rewritten with the new code
  under the IG's code system.
- The clinic's facility-type coding becomes `secondary`. The old display is
  dropped because it described the old code. The concept's `text` is kept.
- `facility_level` has `mirror_to_organization = true`, so the same change is
  written to the clinic's Organization.

A column that was not edited is left as it was. `changes.ndjson` then goes to
the server with `fhir-pipes load`. On a snapshot transformed without the
profile, the same file is refused because `settlement_type` is neither a
built-in column nor in the profile.

## Keeping the profile in step with the IG

When the IG changes, the profile has to change with it.
`fhir-pipes profile check` reports entries that no longer match; see
[Checking a profile against its IG](profiles.md#checking-a-profile-against-its-ig).
A profile for another IG follows
[Writing a profile for your IG](profiles.md#writing-a-profile-for-your-ig).
