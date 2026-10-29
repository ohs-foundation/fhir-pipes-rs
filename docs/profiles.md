# Profiles

A profile is a TOML file that tells fhir-pipes about an implementation guide
(IG): the extra Location and Organization columns to write, the type codes
and extensions that have a special role, and the URLs that the `dhis2`,
`population` and `import` commands put on the resources they create.

Without a profile, fhir-pipes reads base FHIR R4 only. The Location commands
work, but the output has only the built-in columns, with no admin hierarchy
and no country.

[examples/profiles/icr.toml](../examples/profiles/icr.toml) is a complete
profile for the Integrated Campaign Registry (ICR). See
[icr-profile.md](icr-profile.md) for what it reads and writes.

## Choosing a profile

Give the profile as a file path or an https URL. In order of precedence:

1. `--profile FILE|URL` on the command line.
2. The `FHIR_PIPES_PROFILE` environment variable.
3. For `refresh`, `profile = "…"` in `fhir-pipes.toml` (a relative path
   resolves against the config file).
4. The copy kept beside the snapshot.

```sh
fhir-pipes transform locations --profile examples/profiles/icr.toml --snapshot snap --out out
```

`transform locations` and `run` keep a copy as `profile.toml` in the snapshot
directory. `diff`, `index`, `population` and later transforms use that copy
when `--profile` is not given. If `--profile` differs from the copy, the
command uses `--profile` and warns.

## The file

Unknown keys are refused, so a typo is an error. Sections:

| Section | Used for |
|---|---|
| `[location]` | Boundary, spatial index and overlay extensions; `admin_unit_type` (the type code of an administrative unit); `type_systems`; `admin_code_column`; `admin_layers`; `[[location.column]]` |
| `[organization]` | `facility_type`, `id_prefix` (default `org-`), `[[organization.column]]` |
| `[dhis2]` | URLs, systems and extensions that the `dhis2` commands write. Required for `fhir-pipes dhis2` |
| `[population]` | Group profile, characteristic and source systems, extensions. Required for `fhir-pipes population` |

`admin_unit_type` and `admin_code_column` give admin levels, `adminN_*`
columns and `country`. `dhis2 locations` also needs `[location]
admin_unit_type` and `[organization] facility_type`. The ICR profile shows
every key; each key's comment says what uses it.

Commands without the section they need refuse to run, for example
`index needs a profile that names … spatial_index_extension`.

## Column kinds

Each `[[location.column]]` and `[[organization.column]]` has a `name` (lower
case letters, digits and `_`, not a built-in column name), a `kind` and the
keys its kind needs. The value is always text.

| Kind | Keys | Reads |
|---|---|---|
| `identifier` | `systems` | The first identifier under one of the systems, tried in order |
| `extension_code` | `url`, optional `system` | The extension's `valueCode`, or with `system`, the code under that system in its `valueCodeableConcept` |
| `type_coding` | `system`, optional `mirror_to_organization` | The code of the coding under `system` anywhere in `type` |
| `type_text` | `system` | The `text` of the `type` concept that holds a coding under `system` |

```toml
[[location.column]]
name = "pcode"
kind = "identifier"
systems = ["https://example.org/ids/pcode", "https://example.org/ids/national-admin-code"]
```

`diff` writes an edited column value back to the same place. An edited file
with a column that is neither built in nor in the profile is refused, naming
the columns. Use the profile the dataset was made with, or remove the column.

## Writing a profile for your IG

1. Run `fhir-pipes transform locations` without a profile and look at
   `fhir_json` for a few Locations.
2. List what your IG adds: identifier systems, extensions with a code value,
   codings in `type`. Each one you want as a column becomes a column of the
   matching kind. Anything else stays in `fhir_json`.
3. Set `admin_unit_type` and `admin_code_column` if your Locations form an
   admin hierarchy through `partOf`.
4. Transform with `--profile` and check the columns, the `country=`
   partitions and the report.
5. Edit one value per column, run `diff`, and check the changes file. Then
   `load --dry-run`.
6. Add `[dhis2]` or `[population]` only if you use those commands.
7. Run `profile check`, and again whenever the IG changes.

## Checking a profile against its IG

A profile names extensions, code systems and profiles by URL. If the IG
renames or drops one, the profile still loads and its column is empty.
`profile check` compares the two:

```sh
fhir-pipes profile check --profile ig/fhir-pipes-profile.toml --ig ig/fsh-generated/resources
fhir-pipes profile check --profile examples/profiles/icr.toml --ig https://icr.healthcampaigns.org/package.tgz
```

`--ig` is a FHIR package (`package.tgz`, as a path or https URL), a directory
of the IG's JSON resources, or `name#version` for a package in
`~/.fhir/packages`.

It reports errors for extensions, code systems and `meta.profile` URLs that
are missing from the IG or cannot sit on the resource, and for an
`extension_code` column whose `system` does not match the extension's value
type. It reports warnings for identifier systems not sliced in the IG and
codes outside a bound ValueSet. It also lists what it could not check and
what the IG declares that the profile does not read. It exits 1 on any error,
so it can run in the IG's CI.
