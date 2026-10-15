# Profiles

A profile tells fhir-pipes about an implementation guide (IG): the extra
Location and Organization columns to write, the type codes and extensions that
have a special role, and the URLs the `dhis2` and `population` commands put on
the resources they create. It is a TOML file.

Without a profile, fhir-pipes reads base FHIR R4 only. The Location commands
still work, but the output has only the built-in columns, and nothing carries
an admin hierarchy or a country.

[examples/profiles/icr.toml](../examples/profiles/icr.toml) is a complete
profile, for the Integrated Campaign Registry (ICR). ICR's own copy lives with
its IG in recodelabs/icr; the one here is a snapshot used as the example and by
the tests. [icr-profile.md](icr-profile.md) runs it on a small piece of ICR
data and shows what each part reads and writes.

## Contents

- [Choosing a profile](#choosing-a-profile)
- [What a profile changes](#what-a-profile-changes)
- [The file](#the-file)
- [Column kinds](#column-kinds)
- [Editing and diff](#editing-and-diff)
- [Writing a profile for your IG](#writing-a-profile-for-your-ig)
- [Checking a profile against its IG](#checking-a-profile-against-its-ig)
- [Error messages](#error-messages)

## Choosing a profile

Give the profile as a file path or an https URL:

```sh
fhir-pipes transform locations --profile examples/profiles/icr.toml --snapshot snap --out out
fhir-pipes run --profile https://example.org/my-ig/fhir-pipes-profile.toml --server … --snapshot snap --out out
```

The ways to give it, in order of precedence:

1. `--profile FILE|URL` on the command line.
2. The `FHIR_PIPES_PROFILE` environment variable.
3. For `refresh`, `profile = "…"` in `fhir-pipes.toml`. A relative path is
   resolved against the config file.
4. For a command that works from a snapshot, the copy described below.

A URL must be https. The file is fetched with the same timeout and retries as
any other request.

### The copy beside the snapshot

`transform locations` and `run` keep a copy of the profile in the snapshot
directory, as `profile.toml`. When `diff`, `index`, `population` or another
`transform locations` runs on that snapshot without `--profile`, it uses the
copy. So the profile only needs to be given once, and a URL that later
changes or becomes unreachable does not change what `diff` does.

If you give `--profile` and it differs from the copy, the command uses
`--profile` and warns that the snapshot was last transformed with something
else. A fresh snapshot with no copy and no `--profile` uses base FHIR.

Every command that uses a profile prints which one on its first line, for
example `profile: icr (examples/profiles/icr.toml)`.

## What a profile changes

| Command | Without a profile | With a profile |
|---|---|---|
| `transform locations`, `run` | Built-in columns only. `country` is `unknown` unless `--country` is given. No admin levels. | Adds the profile's Location and Organization columns, and the `quadkey` and `overlays_admin_unit_ids` columns when it names those extensions. Admin levels, `adminN_*` and `country` follow its admin units and admin code. |
| `diff` | Writes back the built-in columns. Refuses an edited file with any other column. | Also writes back the profile's columns. |
| `index` | Refuses: it needs the spatial index extension. | Writes cells under `[location] spatial_index_extension`. |
| `population` | Refuses: it needs `[population]`. | Writes Groups with that section's profile and extensions. |
| `dhis2 …` | Refuses: it needs `[dhis2]`. | Mints systems under its base and writes its profiles and extensions. |
| `refresh` | As `run` for the locations step. | As `run` for the locations step. Views and tiles do not use the profile. |
| `extract`, `export`, `load`, `views` | Not used. | Not used. |

The tiles step of `refresh` expects ICR-shaped columns (`pcode`,
`facility_level_text` and so on) and ICR type codes. With another profile, use
`--skip tiles`.

## The file

Every key is optional unless a command that needs it is run. Unknown keys are
refused, so a typo is an error, not a silent no-op.

```toml
name = "my-ig"            # shown in messages
```

### [location]

| Key | Used by | Meaning |
|---|---|---|
| `boundary_extensions` | extract, run, refresh, transform, diff, dhis2 | Boundary attachment extensions to read before the HL7 one. `extract` fetches and inlines an attachment `url` under them as it does under the HL7 one. A new boundary written by `diff` or `dhis2` goes under the first. Any extension whose URL ends in `/StructureDefinition/location-boundary-geojson` is read anyway. |
| `overlays_extension` | transform | A `valueReference` extension listing admin units an area overlaps: the read-only `overlays_admin_unit_ids` column. ICR's IG has none. |
| `spatial_index_extension` | transform, diff, index | The spatial index extension (`system`, `level`, `cell` sub-extensions): the `quadkey` and `quadkey_level` columns, `index`, and the cells `diff` recomputes after a position edit. |
| `admin_unit_type` | transform, population, dhis2 | The Location type code of an administrative unit. `admin_level`, `tier`, `adminN_name`, `adminN_code` and `country` follow the chain of these. |
| `type_systems` | population | Code systems of the Location type codes. `population --type CODE` selects Locations with that code under any of these, wherever the coding sits in `type`. Empty or absent: a code under any system. |
| `admin_code_column` | transform | The name of a `[[location.column]]` holding an admin unit's code. It fills `adminN_code`, gives `country` (from the level 0 unit), and is checked for duplicates (`duplicate_<column>` in the report). |
| `[[location.column]]` | transform, diff | An extra column; see [Column kinds](#column-kinds). Columns appear in this order, after `position_latitude`. |

### [organization]

The Organization a Location names as its `managingOrganization` is projected
onto the Location's row.

| Key | Used by | Meaning |
|---|---|---|
| `facility_type` | diff, dhis2 | A Location of this type that `diff` creates without a `managingOrganization` gets a new Organization. `dhis2 locations` gives facilities this type. |
| `id_prefix` | diff, dhis2 | The id of such an Organization: `<id_prefix><location id>`. Default `org-`. |
| `[[organization.column]]` | transform, diff | An extra column read from the Organization, after `organization_identifier`. |

### [dhis2]

All required for `fhir-pipes dhis2`.

| Key | Meaning |
|---|---|
| `base` | Systems are minted under it: `<base>/identifiers/dhis2/<instance>/orgunit`, `<base>/CodeSystem/dhis2/<instance>/…`, and Measure URLs `<base>/Measure/<id>`. |
| `project_tag_system` | The `meta.tag` system marking every resource with its instance. |
| `location_profile`, `facility_organization_profile` | `meta.profile` of the Locations and facility Organizations written. |
| `location_type_system` | The system of the Location type codes written (`admin_unit_type`, `facility_type`). |
| `country_code_system` | The identifier system `--country-code` is written under on the top org unit. Make it one the admin code column reads, so it becomes the country. |
| `period_extension`, `campaign_extension`, `source_extension` | Extensions on the MeasureReports written. |
| `indicator_extension` | The extension on the Measures written. |

`dhis2 locations` also needs `[location] admin_unit_type` and
`[organization] facility_type`.

### [population]

All required for `fhir-pipes population`, except the last two.

| Key | Meaning |
|---|---|
| `group_profile` | `meta.profile` of the Groups written. |
| `characteristic_system` | The system of the `geography` characteristic code. |
| `denominator_source_system` | The system of the `--source` code. |
| `denominator_source_extension`, `denominator_type_extension`, `estimate_date_extension`, `is_planning_denominator_extension`, `is_calculated_extension` | The extensions on each Group. |
| `known_source_codes` | Codes of the source system. A `--source` outside them is reported as `unknown_source_code` and written anyway. |
| `source_displays` | Display text for source codes, for example `{ worldpop = "WorldPop modelled estimate" }`. |

## Column kinds

Each `[[location.column]]` and `[[organization.column]]` has a `name` (lower
case letters, digits and `_`, and not a built-in column name), a `kind`, and
the keys its kind needs. The value is always text.

### identifier

The value of the first identifier under one of `systems`, tried in order.

```toml
[[location.column]]
name = "pcode"
kind = "identifier"
systems = ["https://example.org/ids/pcode", "https://example.org/ids/national-admin-code"]
```

`identifier: [{system: …/national-admin-code, value: "NG"}]` → `pcode = "NG"`.

**diff** writes an edit to the system the resource already uses, else to the
first of `systems`, and removes that identifier when the value is cleared.
The whole `identifier` list column, if also edited, is applied first.

### extension_code

The code in the extension with `url`: its `valueCode`, or with `system`, the
code under that system in its `valueCodeableConcept`.

```toml
[[location.column]]
name = "access"
kind = "extension_code"
url = "https://example.org/fhir/StructureDefinition/access"
```

`extension: [{url: …/access, valueCode: "seasonal"}]` → `access = "seasonal"`.

When the IG gives the extension a CodeableConcept value, name its code system:

```toml
[[location.column]]
name = "settlement_type"
kind = "extension_code"
url = "https://example.org/fhir/StructureDefinition/settlement-type"
system = "https://example.org/fhir/CodeSystem/settlement-type"
```

`extension: [{url: …/settlement-type, valueCodeableConcept: {coding: [{system:
…/CodeSystem/settlement-type, code: "urban"}]}}]` → `settlement_type =
"urban"`.

**diff** leaves the extension alone when the value is unchanged. An edit sets
`valueCode`, or with `system` replaces the coding under that system in
`valueCodeableConcept`. It keeps other systems' codings and drops the old
coding's display and the concept text. A missing extension is added.
Clearing the value removes the extension; with `system`, it removes that
system's coding, and the extension once no coding is left.

### type_coding

The code of the coding under `system` anywhere in `type` (a list of
CodeableConcepts).

```toml
[[location.column]]
name = "ownership"
kind = "type_coding"
system = "https://example.org/fhir/CodeSystem/ownership"
mirror_to_organization = true
```

`type: [{coding: [{system: …/ownership, code: "public"}]}]` →
`ownership = "public"`.

**diff** sets that coding's code, adds a concept holding it if missing, and
removes the coding when cleared. A new code drops the coding's old `display`. With `mirror_to_organization = true`, an
edit is also written to the paired Organization's type: always for a new
row, and for an existing one only when the value was edited.

### type_text

The `text` of the concept in `type` that holds a coding under `system`, such
as a human-readable label next to a code.

```toml
[[organization.column]]
name = "ownership_text"
kind = "type_text"
system = "https://example.org/fhir/CodeSystem/ownership"
```

**diff** sets or clears that concept's `text`. It cannot create the coding:
an edit to a resource without one is reported as `input_column_type` and
ignored.

## Editing and diff

`diff` turns an edited GeoJSON or GeoParquet file back into FHIR. It writes
the built-in writable columns and the profile's columns, and ignores the
built-in derived ones (`admin1_name`, `path`, `country` and so on).

An edited file with a column that is neither built in nor in the profile is
refused, naming the columns:

```
the edited file has column(s) pcode that are neither built-in columns nor in the
profile (no profile); pass the profile the dataset was made with (--profile), or
remove the column(s)
```

Without that check, editing ICR's `pcode` with no profile would silently
change nothing. Use the profile the dataset was made with (normally the
snapshot's copy is enough), or remove a column you added yourself.

## Writing a profile for your IG

1. **Start from base.** Run `fhir-pipes transform locations` without a
   profile and look at `fhir_json` for a few Locations, for example with
   `fhir-pipes inspect` or DuckDB.
2. **List what your IG adds to Location and Organization**: identifier
   systems, extensions with a code or CodeableConcept value, and codings in
   `type`. Each one you
   want as a column becomes a `[[location.column]]` or
   `[[organization.column]]` of the matching kind. Anything else stays in
   `fhir_json`, which is lossless.
3. **Name the roles.** If your Locations form an admin hierarchy through
   `partOf`, set `admin_unit_type` to the type code of an admin unit, and
   `admin_code_column` to the column holding its code, to get admin levels and
   countries. Add `boundary_extensions` only if your boundary extension's URL
   does not end in `/StructureDefinition/location-boundary-geojson`.
4. **Transform and check.** Run with `--profile` and check the columns, the
   `country=` partitions and the report. `duplicate_<column>` and
   `no_country` point at data or profile problems.
5. **Round trip one edit per column.** Edit a value in QGIS or DuckDB, run
   `diff`, and check the resource in the changes file has it where your IG
   expects it. Then `load --dry-run`.
6. **Add `[dhis2]` or `[population]`** only if you use those commands; copy
   the ICR example's keys and replace the URLs with your IG's.
7. **Check it against the IG** with `fhir-pipes profile check` (below), and
   again whenever the IG changes.
8. **Publish it** next to your IG, for example at a stable https URL, so
   others can pass `--profile https://…`.

## Checking a profile against its IG

A profile names extensions, code systems and profiles by URL. When the IG
renames one, changes its value type or drops it, the profile still loads and
its column comes out empty. `profile check` compares the two:

```sh
fhir-pipes profile check --profile ig/fhir-pipes-profile.toml --ig ig/fsh-generated/resources
fhir-pipes profile check --profile examples/profiles/icr.toml --ig https://icr.healthcampaigns.org/package.tgz
```

`--ig` is a FHIR package (`package.tgz`, as a path or an https URL) or a
directory of the IG's JSON resources: SUSHI's `fsh-generated/resources`, the
IG publisher's `output`, or an unpacked package. A published package is only
as current as the last IG build; SUSHI's output follows the FSH you have.

For each URL under the IG's canonical, the check reports:

| Problem | Severity |
|---|---|
| An extension the profile names is not in the IG | error |
| An extension cannot sit on the resource it is read from: its `context` is, say, Task, or an element below the root such as `Location.position`. A FHIRPath context other than a list of types (`Location \| Organization`) is listed as not checked | error |
| An `extension_code` column without `system` on an extension whose value is a CodeableConcept, or with `system` on one whose value is a `code` or a `Coding` (fhir-pipes reads `valueCodeableConcept`, not `valueCoding`) | error |
| An `extension_code` column's `system` is not in the ValueSet the extension is bound to, following the IG's ValueSets it imports. A binding that leads outside the IG is not judged | warning |
| A code system the profile names is not in the IG, or `admin_unit_type`, `facility_type` or a `known_source_codes` entry is not a code in it | error |
| A later `type_systems` entry (an older URI) is not in the IG | warning |
| A `meta.profile` (`location_profile`, `group_profile` …) is not in the IG, or constrains another type | error |
| An identifier system is not sliced in any of the IG's profiles of that resource type (by a `system` pattern on the slice's `system`, or a `patternIdentifier` on the slice) | warning |
| The IG's denominator source code system has codes `known_source_codes` lacks | warning |

It then lists what it could not check: URLs outside the IG, and identifier
systems on a resource type whose profiles slice none. It also lists the
extensions and identifier systems the IG's Location and Organization
profiles declare that the profile does not read, which are candidates for
new columns.

`profile check` exits 1 if there is any error, so it can run in the IG's CI.
For ICR's profile against its current IG:

```
error: [dhis2] source_extension: https://icr.healthcampaigns.org/StructureDefinition/dhis2-source is not an extension in the IG
warning: [location] type_systems: https://icr.healthcampaigns.org/CodeSystem/icr-location-type is not a CodeSystem in the IG
warning: [[location.column]] pcode: https://icr.healthcampaigns.org/identifiers/national-admin-code is not an identifier slice in any of the IG's Location profiles
warning: [dhis2] country_code_system: https://icr.healthcampaigns.org/identifiers/national-admin-code is not an identifier slice in any of the IG's Location profiles
not checked:
  https://icr.healthcampaigns.org/identifiers/nga-nhfr-code (the IG's Organization profiles slice no identifier systems)
  https://icr.healthcampaigns.org/identifiers/nga-nhfr-uid (the IG's Organization profiles slice no identifier systems)
in the IG's Location profiles but not read by the profile:
  https://icr.healthcampaigns.org/StructureDefinition/building-count
  https://icr.healthcampaigns.org/StructureDefinition/catchment-of
  urn:iso:std:iso:3166
  urn:iso:std:iso:3166:-2
1 error(s), 3 warning(s)
```

## Error messages

| Message | Meaning |
|---|---|
| `profile …: unknown field …` | A key that does not exist, often a typo. |
| `profile …: unknown variant …` | A `kind` that is not one of the four. |
| `column … needs systems / url / system` | The column's kind needs that key. |
| `column … has a key that kind does not use` | For example `url` on an `identifier` column. |
| `… is a built-in column` | Choose another name; built-in names are reserved. |
| `admin_code_column … is not a [[location.column]]` | Declare the column, or fix the name. |
| `profiles are fetched over https only` | Use an https URL, or download the file. |
| `the edited file has column(s) …` | See [Editing and diff](#editing-and-diff). |
| `… needs a profile with a [dhis2] / [population] section` | Pass a profile that has it. |
| `index needs a profile that names … spatial_index_extension` | Pass a profile that names it. |
| `warning: --profile … differs from …/profile.toml` | The snapshot was last transformed with a different profile. `diff` uses `--profile`; re-run `transform locations` if the dataset should change too. |
