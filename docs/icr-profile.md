# The ICR profile, worked through

This guide runs fhir-pipes on a small piece of ICR data and shows, for each
part of ICR's profile, the FHIR it reads and the columns it produces. The
reference for every key is [profiles.md](profiles.md).

The Integrated Campaign Registry (ICR) is a FHIR implementation guide (IG) for
health campaigns. Its profile lives with the IG, at
[`ig/fhir-pipes-profile.toml`](https://github.com/recodelabs/icr/blob/main/ig/fhir-pipes-profile.toml)
in recodelabs/icr. [examples/profiles/icr.toml](../examples/profiles/icr.toml)
is a copy of it.

## Contents

- [The example data](#the-example-data)
- [Without the profile](#without-the-profile)
- [With the profile](#with-the-profile)
- [Admin units and countries](#admin-units-and-countries)
- [Facilities and their Organizations](#facilities-and-their-organizations)
- [Settlements](#settlements)
- [Spatial index](#spatial-index)
- [Boundaries](#boundaries)
- [What stays in fhir_json](#what-stays-in-fhir_json)
- [Writing edits back with diff](#writing-edits-back-with-diff)
- [population and dhis2](#population-and-dhis2)
- [How the profile maps to the IG](#how-the-profile-maps-to-the-ig)

## The example data

[examples/icr/snapshot](../examples/icr/snapshot) is a snapshot of six
Locations and one Organization taken from ICR's server: one branch of the
Nigerian admin tree and what hangs off it.

| id | name | Location type |
|---|---|---|
| `nga` | Nigeria | `admin-unit` |
| `nga-ba` | Bauchi (a state) | `admin-unit` |
| `nga-ba-5018` | Toro (a local government area) | `admin-unit` |
| `2d14d838-…` | Makera Primary Health Clinic | `facility` |
| `catchment-2d14d838-…` | Makera Primary Health Clinic catchment | `facility-catchment` |
| `e164b9cb-…` | Gidan Shuaibu (a settlement) | `settlement` |

`organizations.ndjson` holds the clinic's Organization,
`org-2d14d838-…`.

Two things differ from the live data. The boundaries are cut down to their
bounding boxes to keep the files small. And the settlement has a
`settlement-type` extension, which the IG defines but ICR's data does not yet
carry.

## Without the profile

Transform keeps a copy of the profile it used beside the snapshot, so work on
copies of the example rather than the files in the repository:

```sh
cp -r examples/icr/snapshot /tmp/base-snap
cp -r examples/icr/snapshot /tmp/snap
fhir-pipes transform locations --snapshot /tmp/base-snap --out /tmp/base
```

```
profile: base (no profile)
indexed 6 resources, 6 with geometry and hierarchy
…
Wrote 6 rows across 4 partitions to /tmp/base
  locations/country=unknown/geom_type=point/type=settlement/part-0.parquet (1 rows)
  locations/country=unknown/geom_type=point/type=facility/part-0.parquet (1 rows)
  locations/country=unknown/geom_type=polygon/type=admin-unit/part-0.parquet (3 rows)
  locations/country=unknown/geom_type=polygon/type=facility-catchment/part-0.parquet (1 rows)
Issues found:
  small_partition: 4
```

`small_partition` only says the partitions are small, as they are with six
rows.

Base FHIR has no notion of an admin unit or a country, so every row is
`country=unknown` and the admin columns are empty:

| name | type | admin_level | admin1_name | admin2_name | country |
|---|---|---|---|---|---|
| Nigeria | admin-unit | | | | unknown |
| Bauchi | admin-unit | | | | unknown |
| Toro | admin-unit | | | | unknown |
| Gidan Shuaibu | settlement | | | | unknown |
| Makera Primary Health Clinic | facility | | | | unknown |
| Makera Primary Health Clinic catchment | facility-catchment | | | | unknown |

The rows have only the built-in columns: `id`, `name`, `type`, `part_of`,
`identifier`, the position, `organization_identifier`, the admin and path
columns, `geometry` and `fhir_json`.

## With the profile

```sh
fhir-pipes transform locations --profile examples/profiles/icr.toml \
  --snapshot /tmp/snap --out /tmp/icr
```

```
profile: icr (examples/profiles/icr.toml)
indexed 6 resources, 6 with geometry and hierarchy
…
Wrote 6 rows across 4 partitions to /tmp/icr
  locations/country=NGA/geom_type=point/type=settlement/part-0.parquet (1 rows)
  locations/country=NGA/geom_type=point/type=facility/part-0.parquet (1 rows)
  locations/country=NGA/geom_type=polygon/type=admin-unit/part-0.parquet (3 rows)
  locations/country=NGA/geom_type=polygon/type=facility-catchment/part-0.parquet (1 rows)
Issues found:
  small_partition: 4
```

The rows now sit under `country=NGA`, and these columns are added:

- after `position_latitude`: `pcode`, `gers_id`, `settlement_type`,
  `facility_level`, `ownership`, `quadkey`, `quadkey_level`
- after `organization_identifier`: `nhfr_code`, `nhfr_uid`,
  `facility_level_text`, `ownership_text`

Transform also kept a copy of the profile as `/tmp/snap/profile.toml`, so
later commands on this snapshot use ICR's profile without `--profile`.

The sections below take the profile a part at a time.

## Admin units and countries

```toml
[location]
admin_unit_type = "admin-unit"
admin_code_column = "pcode"

[[location.column]]
name = "pcode"
kind = "identifier"
systems = [
    "https://icr.healthcampaigns.org/identifiers/pcode",
    "https://icr.healthcampaigns.org/identifiers/national-admin-code",
]
```

Toro, as FHIR:

```json
{
  "id": "nga-ba-5018",
  "name": "Toro",
  "identifier": [{
    "system": "https://icr.healthcampaigns.org/identifiers/national-admin-code",
    "value": "5018"
  }],
  "type": [{"coding": [{
    "system": "https://icr.healthcampaigns.org/CodeSystem/icr-location-type-cs",
    "code": "admin-unit"
  }]}],
  "partOf": {"reference": "Location/nga-ba"}
}
```

- `admin_unit_type` says a Location typed `admin-unit` is an admin unit.
  fhir-pipes follows `partOf` up through admin units, so Nigeria is level 0,
  Bauchi level 1 and Toro level 2.
- `pcode` is an `identifier` column with two systems, tried in order. Toro has
  no `…/pcode` identifier, so the value comes from `…/national-admin-code`:
  `5018`.
- `admin_code_column = "pcode"` makes that column the admin code. It fills
  `adminN_code`. Nigeria's code, `NGA`, becomes `country`, and so the
  `country=NGA` partition. Two admin units with the same code are reported as
  `duplicate_pcode`.

Rows below an admin unit, such as the settlement, facility and catchment,
take the names and codes of the units above them:

| name | type | admin_level | tier | admin1_name | admin2_name | admin2_code | country |
|---|---|---|---|---|---|---|---|
| Nigeria | admin-unit | 0 | 0 | | | | NGA |
| Bauchi | admin-unit | 1 | 1 | Bauchi | | | NGA |
| Toro | admin-unit | 2 | 2 | Bauchi | Toro | 5018 | NGA |
| Gidan Shuaibu | settlement | | site | Bauchi | Toro | 5018 | NGA |
| Makera Primary Health Clinic | facility | | site | Bauchi | Toro | 5018 | NGA |
| Makera Primary Health Clinic catchment | facility-catchment | | site | Bauchi | Toro | 5018 | NGA |

## Facilities and their Organizations

In ICR a facility is a Location for the place and an Organization for the
provider, linked by `managingOrganization`. Each carries the facility level
and ownership as codings in `type`, and the Organization carries the national
health facility registry (NHFR) identifiers.

The clinic's Location:

```json
{
  "id": "2d14d838-685c-4723-9f73-06392f7ff353",
  "type": [
    {"coding": [{"system": "https://icr.healthcampaigns.org/CodeSystem/icr-location-type-cs",
                 "code": "facility"}]},
    {"coding": [{"system": "https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs",
                 "code": "primary", "display": "Primary"}],
     "text": "Primary Health Clinic"},
    {"coding": [{"system": "https://icr.healthcampaigns.org/CodeSystem/icr-ownership-cs",
                 "code": "public", "display": "Public"}],
     "text": "Local Government"}
  ],
  "managingOrganization": {"reference": "Organization/org-2d14d838-685c-4723-9f73-06392f7ff353"}
}
```

Its Organization:

```json
{
  "id": "org-2d14d838-685c-4723-9f73-06392f7ff353",
  "identifier": [
    {"system": "https://icr.healthcampaigns.org/identifiers/nga-nhfr-code", "value": "05/18/1/1/1/0042"},
    {"system": "https://icr.healthcampaigns.org/identifiers/nga-nhfr-uid", "value": "86505768"}
  ],
  "type": [ "… the same facility-type and ownership concepts …" ]
}
```

The profile reads the codes from the Location and the identifiers and labels
from the Organization:

```toml
[[location.column]]
name = "facility_level"
kind = "type_coding"
system = "https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs"
mirror_to_organization = true

[[location.column]]
name = "ownership"
kind = "type_coding"
system = "https://icr.healthcampaigns.org/CodeSystem/icr-ownership-cs"
mirror_to_organization = true

[[organization.column]]
name = "nhfr_code"
kind = "identifier"
systems = ["https://icr.healthcampaigns.org/identifiers/nga-nhfr-code"]

[[organization.column]]
name = "facility_level_text"
kind = "type_text"
system = "https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs"
```

`nhfr_uid` and `ownership_text` follow the same pattern.

| name | facility_level | ownership | nhfr_code | nhfr_uid | facility_level_text | ownership_text |
|---|---|---|---|---|---|---|
| Makera Primary Health Clinic | primary | public | 05/18/1/1/1/0042 | 86505768 | Primary Health Clinic | Local Government |

- A `type_coding` column takes the code under its system, wherever that
  concept sits in `type`.
- A `type_text` column takes the `text` of the concept holding that system's
  coding, here the registry's label.
- `mirror_to_organization` matters when editing. See
  [Writing edits back with diff](#writing-edits-back-with-diff).

The `[organization]` section also says how `diff` creates the Organization for
a new facility: a new Location of type `facility` gets an Organization with
id `org-<location id>`, the pattern ICR uses.

```toml
[organization]
facility_type = "facility"
id_prefix = "org-"
```

## Settlements

```toml
[[location.column]]
name = "settlement_type"
kind = "extension_code"
url = "https://icr.healthcampaigns.org/StructureDefinition/settlement-type"
system = "https://icr.healthcampaigns.org/CodeSystem/icr-settlement-type-cs"
```

The IG defines `settlement-type` with a CodeableConcept value, so the column
names the code system and reads the code under it:

```json
{
  "url": "https://icr.healthcampaigns.org/StructureDefinition/settlement-type",
  "valueCodeableConcept": {"coding": [{
    "system": "https://icr.healthcampaigns.org/CodeSystem/icr-settlement-type-cs",
    "code": "rural", "display": "Rural"
  }]}
}
```

That gives `settlement_type = "rural"` for Gidan Shuaibu. An extension with a
plain `valueCode` would use an `extension_code` column without `system`.

## Spatial index

```toml
[location]
spatial_index_extension = "https://icr.healthcampaigns.org/StructureDefinition/spatial-index"
```

ICR keeps a quadkey cell on each point Location:

```json
{
  "url": "https://icr.healthcampaigns.org/StructureDefinition/spatial-index",
  "extension": [
    {"url": "system", "valueCode": "quadkey"},
    {"url": "level", "valueUnsignedInt": 18},
    {"url": "cell", "valueString": "122221103102030132"}
  ]
}
```

That gives `quadkey = 122221103102030132` and `quadkey_level = 18`. The same
key lets `fhir-pipes index` compute cells for every Location, and lets `diff`
recompute the cell when a position is edited. Without it, `index` refuses to
run and the two columns are absent.

## Boundaries

```toml
[location]
boundary_extensions = [
    "https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson",
]
```

ICR's admin units and catchments carry their boundary as GeoJSON in this
extension's `valueAttachment`. Its URL ends in
`/StructureDefinition/location-boundary-geojson`, so fhir-pipes would read it
even without the profile. Naming it in the profile still matters in two places:

- When `diff` writes a new or moved boundary, it goes under ICR's extension
  rather than the HL7 one.
- `extract` downloads an attachment given as a `url` under this extension and
  inlines it.

## What stays in fhir_json

Anything the profile does not name is kept unchanged in `fhir_json`, and
identifiers also appear in the `identifier` list column. In this data:

- The settlement's GRID3 identifiers (`grid3-globalid`,
  `grid3-settlement-id`) appear only in `identifier`.
- The catchment's `catchment-of` reference and its `building-count`
  extensions are in `fhir_json` only. (`catchment_extension` names
  `catchment-of` for the catchment tiles `refresh` builds, not for a
  column.)
- `gers_id` is declared but empty: none of these Locations has an Overture
  GERS identifier.

To make one of these a column, add it to the profile. For example, a GRID3 id
column would be another `identifier` column.

## Writing edits back with diff

Suppose the settlement turns out to be hard to reach, and the clinic is
upgraded. Here is the edit as GeoJSON, with only the changed properties. A
GIS edit of the full dataset works the same way.

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

```
profile: icr (/tmp/snap/profile.toml)
indexed 6 snapshot resources
indexed 1 organizations
2 changed, 0 unchanged, 0 new -> changes.ndjson
Location: 2, Organization: 1
No issues found.
```

`diff` takes the profile from the copy transform left in `/tmp/snap` and
writes each edit where the column reads it from:

- **The settlement:** the extension is rewritten in the IG's shape:
  ```json
  {"url": "https://icr.healthcampaigns.org/StructureDefinition/settlement-type",
   "valueCodeableConcept": {"coding": [{
     "system": "https://icr.healthcampaigns.org/CodeSystem/icr-settlement-type-cs",
     "code": "hard-to-reach"}]}}
  ```
- **The clinic's Location:** the facility-type coding becomes
  `secondary`. The old display, "Primary", is dropped because it described
  the old code. The concept's `text` is kept, since it belongs to
  `facility_level_text`:
  ```json
  {"coding": [{"system": "https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs",
               "code": "secondary"}],
   "text": "Primary Health Clinic"}
  ```
- **The clinic's Organization:** `facility_level` has
  `mirror_to_organization = true`, so the same coding change is written there
  too. That is why the output has one Organization.

A column whose value was not edited is left as it was. Passing a whole
untouched Parquet partition back through `diff` reports `0 changed`, and the
settlement keeps its `display: "Rural"`. Only an edited code rewrites the
coding.

`changes.ndjson` then goes to the server with `fhir-pipes load`.

On a snapshot transformed without the profile, the same file is refused
rather than half applied. `diff` stops at the first row with a column it does
not know:

```
fhir-pipes: the edited file has column(s) settlement_type that are neither
built-in columns nor in the profile (/tmp/base-snap/profile.toml); pass the
profile the dataset was made with (--profile), or remove the column(s)
```

## population and dhis2

The profile's other sections are for commands that create resources.

`type_systems` lists the code systems of ICR's Location types, the canonical
one and an older URI some data still carries:

```toml
type_systems = [
    "https://icr.healthcampaigns.org/CodeSystem/icr-location-type-cs",
    "https://icr.healthcampaigns.org/CodeSystem/icr-location-type",
]
```

`population --type facility-catchment` selects Locations with that code under
either system, wherever the coding sits in `type`. In this data, that is the
clinic's catchment.

`[population]` gives the URLs on the Groups that command writes:
`group_profile` is ICR's `ICRTargetPopulation`, plus the systems and
extensions for the denominator source, type and date. `known_source_codes`
lists the IG's denominator source codes, so a `--source` the IG does not
define is reported.

`[dhis2]` does the same for `fhir-pipes dhis2`. It sets the base the DHIS2
org unit identifiers are minted under (`https://icr.healthcampaigns.org`),
the `ICRLocation` and `ICRFacilityOrganization` profiles put on what it
writes, and the MeasureReport extensions.

## How the profile maps to the IG

Each profile entry points at something the IG defines. When the IG changes,
the profile has to change with it.

| Profile entry | ICR IG definition |
|---|---|
| `admin_unit_type`, `type_systems` | CodeSystem `icr-location-type-cs` (code `admin-unit`) |
| `pcode` | Identifier systems `…/identifiers/pcode`, `…/identifiers/national-admin-code` |
| `gers_id` | Identifier system `…/identifiers/overture-gers` |
| `settlement_type` | Extension `settlement-type` (CodeableConcept from ICRSettlementTypeVS) |
| `facility_level`, `facility_level_text` | CodeSystem `icr-facility-type-cs` |
| `ownership`, `ownership_text` | CodeSystem `icr-ownership-cs` |
| `nhfr_code`, `nhfr_uid` | Identifier systems `…/identifiers/nga-nhfr-code`, `…/identifiers/nga-nhfr-uid` |
| `spatial_index_extension` | Extension `spatial-index` |
| `catchment_extension` | Extension `catchment-of` |
| `boundary_extensions` | Extension `location-boundary-geojson` |
| `[dhis2]`, `[population]` | The ICR profiles, CodeSystems and extensions named there |

Some things the IG has are not in the profile on purpose:

- **`delivery-strategy`:** an extension on PlanDefinition, ActivityDefinition
  and Task, not Location. The IG keeps a site's delivery strategy in each
  campaign's Tasks.
- **`catchment-of` and `building-count`:** a reference and structured
  extensions, not one code. They stay in `fhir_json`; `catchment-of` is also
  `catchment_extension`, for the catchment tiles.

`fhir-pipes profile check` compares the profile with the IG and reports an
entry that no longer matches. See
[Checking a profile against its IG](profiles.md#checking-a-profile-against-its-ig).

A profile for another IG follows the same steps. See
[Writing a profile for your IG](profiles.md#writing-a-profile-for-your-ig).
