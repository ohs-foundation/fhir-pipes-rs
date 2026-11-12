# DHIS2 import

Reads a DHIS2 instance (Web API, 2.38 or later) and writes what it reads as
records, which an import package turns into FHIR NDJSON for
`fhir-pipes load`. Nothing is written to DHIS2. See
[locations.md](locations.md) for the Location registry the org units become.

```
fhir-pipes dhis2 extract locations|measures|reports [OPTIONS] --out RAW
fhir-pipes import run --package examples/import/icr-dhis2 --in RAW --out FHIR
```

`dhis2 extract` writes one file of records per command in the folder `RAW`.
`import run` maps the folder with the package
[examples/import/icr-dhis2](../examples/import/icr-dhis2) and writes one
NDJSON file per resource type. `dhis2 extract` is the example connector of
[connectors.md](connectors.md).

`dhis2 locations`, `dhis2 measures` and `dhis2 reports` do both steps in one
command, for one record type, and write one NDJSON file of resources. They
need a package; see
[The commands that write resources](#the-commands-that-write-resources).

`--help` lists every option.

## Common options

| Option | Meaning |
|---|---|
| `--dhis2`, `$FHIR_PIPES_DHIS2_URL` | The instance's base URL; a trailing `/api` is fine |
| `--dhis2-token`, `$FHIR_PIPES_DHIS2_TOKEN` | Personal access token, sent as `Authorization: ApiToken …` |
| `--dhis2-user` / `--dhis2-password`, `$FHIR_PIPES_DHIS2_USER` / `$FHIR_PIPES_DHIS2_PASSWORD` | Basic authentication |
| `--instance` | A short name for the instance: letters, digits and hyphens, up to 16 characters, such as `sl-demo` |
| `--report FILE` | Also write the run report as JSON |

Use a read-only DHIS2 user. Keep `--instance` the same across runs: it
prefixes every id and is in every record, and ids are deterministic, so a
re-run replaces the same resources.

No `dhis2` command reads the profile. The systems, profiles and extensions
the resources carry are parameters of the package (see
[import.md](import.md#parameters)).

## The records

```
fhir-pipes dhis2 extract locations|measures|reports [OPTIONS] --out DIR
```

`--out` is a folder, created if missing. Each command writes one file in it,
named after its record type, so three runs into one folder give the input
of one `import run`:

| Command | Record type | File |
|---|---|---|
| `dhis2 extract locations` | `DHIS2OrgUnit`: one per org unit | `DIR/DHIS2OrgUnit.ndjson` |
| `dhis2 extract measures` | `DHIS2Indicator`: one per indicator, data element or reporting rate | `DIR/DHIS2Indicator.ndjson` |
| `dhis2 extract reports` | `DHIS2Value`: one per value, with its breakdowns | `DIR/DHIS2Value.ndjson` |

The records' logical models are in
[examples/import/icr-dhis2/models](../examples/import/icr-dhis2/models).
fhir-pipes prepares each record from the DHIS2 API: the ids, the parent only
when it was exported, a boundary merged from the children, counts as whole
numbers, breakdowns attached to their totals. The records carry none of the
URLs, profiles, extensions or type codes of the resources: the package's
maps write them.

Each record file is written under a temporary name and renamed when the run
ends. An interrupted `dhis2 extract reports` leaves `DHIS2Value.ndjson.tmp`
and `DHIS2Value.ndjson.resume` in the folder for `--resume`; `import run`
reads only `*.ndjson` and `*.ndjson.gz` files, so it does not read them.
`--report FILE` is a file of your choosing; put it outside the folder, or
name it so that it does not end in `.ndjson`.

## dhis2 extract locations

Reads the org unit tree. Levels above the facility level are admin units.
The facility level (`--facility-level N`, default the deepest) is facilities.
Org units below it are skipped. The package's map turns an admin unit into a
Location, and a facility into a Location and an Organization
(`managingOrganization` is `org-<location id>`). Location ids are
`PREFIX-<uid>`.

| Option | Meaning |
|---|---|
| `--id-prefix` | Id prefix. Default: the instance name |
| `--country-code` | A national admin code for the top org unit; `transform locations` partitions by it (`SLE`, `NGA`) |
| `--facility-level` | The org unit level of the facilities |
| `--root UID` | Read one subtree |
| `--group-set UID`, `--org-unit-group UID`, `--all-org-unit-groups` | Add org unit groups to each record, for the Location's `type` |

A Polygon or MultiPolygon becomes the record's `boundary` and a Point its
`longitude` and `latitude`. An admin unit with no geometry gets the union of
its children's polygons.

## dhis2 extract measures

Reads indicators. Choose them with `--indicator-group` and `--indicator`
(both repeatable). `--data-element-group`, `--data-element`,
`--data-element-group-set` and `--data-set` add data elements (counts) and
data set reporting rates. The Measure id the package writes is
`NAME-<uid>`.

A percent indicator (factor 100) is scored `proportion`, another factor
`ratio`, and a denominator of `1` `cohort`.

## dhis2 extract reports

Asks `/api/analytics` for the selected indicators at each `--level` and
`--period`, and writes one record per indicator, org unit and period, with
id `NAME-<indicator>-<orgunit>-<period>`. The package's map writes one
`summary` MeasureReport per record, with the same id.

DHIS2 computes every value. fhir-pipes never sums or averages indicator
values, so ask for each level you want. Values reflect the last analytics
table run, which is printed at the start with a warning when it is over 7
days old. `--max-analytics-age 7d` refuses the run instead.

| Option | Meaning |
|---|---|
| `--period` (repeatable) | ISO (`2025`, `2025Q1`, `202501`) or relative (`LAST_12_MONTHS`) |
| `--range START:END` (repeatable) | One value over a date range, both dates included, such as `2026-04-05:2026-05-20`. Can be used with or instead of `--period` |
| `--level` (repeatable) | Org unit levels to report |
| `--root UID` | Only org units in this tree. Use it when the DHIS2 user sees top-level org units outside it, which DHIS2 otherwise refuses (E7120) |
| `--breakdown UID` (repeatable) | Break values down by a category, org unit group set or data element group set, as `group.stratifier` |
| `--campaign TYPE/ID` | Adds the campaign, for example `CarePlan/bgd-mr-sia-2026`. Report ids do not include the campaign |
| `--source` | `analytics` (default) or `data-values` |
| `--org-units FILE` | The `DHIS2OrgUnit` records, below |

`extract reports` finds each org unit in the `DHIS2OrgUnit` records that
`extract locations` wrote: `--org-units FILE` (NDJSON, or `.ndjson.gz`), or
without it `DHIS2OrgUnit.ndjson` (or `.ndjson.gz`) in `--out`. So
`extract locations` then `extract reports` into one folder needs no extra
option. With neither, the command stops with exit 2. Only the records of
`--instance` are read. From each record it takes the org unit's UID, the id
of its Location (`<idPrefix>-<uid>`, as the package's map writes it), its
name, and for `--source data-values` its parent.

An org unit that `extract locations` left out (below the facility level, or
with an id that is not a valid FHIR id) has no record. Its values are left
out and reported as `unmapped_org_unit`.

### Resuming

`--resume` continues a failed or stopped run from `OUT.ndjson.resume`. It is
refused if the arguments (other than `--concurrency`), the DHIS2 URL, an org
unit count, the analytics build time, or the file the org units were read
from has changed. A run of `dhis2 extract reports` is not resumed by
`dhis2 reports`, nor the other way round. Run without `--resume` to start
over.

### Computing values from data values

`--source data-values` reads raw data values and computes each value itself,
in the same shape as analytics. Only SUM and AVERAGE_SUM_ORG_UNIT data
elements and simple indicator expressions are supported; anything else is
refused, naming the indicator. The org unit records (`--locations` for
`dhis2 reports`) must then include the data-entry units and their complete
parent chains up to the requested levels, or totals are undercounted. Org
units below the facility level have no record and no Location, so values
entered there are left out. A record's `date` is when the run started.

## Mapping the records

```sh
fhir-pipes dhis2 extract locations --instance sl-demo --id-prefix sle --country-code SLE \
  --out raw
fhir-pipes dhis2 extract measures --instance sl-demo --indicator-group srxUNt2w5jn \
  --out raw
fhir-pipes dhis2 extract reports --instance sl-demo --indicator-group srxUNt2w5jn \
  --period 2025 --level 2 --level 3 --out raw
fhir-pipes import run --package examples/import/icr-dhis2 --in raw --out fhir
for t in Organization Location Measure MeasureReport; do
  fhir-pipes load --server URL --in fhir/$t.ndjson
done
```

Load Organizations before Locations: a facility Location refers to its
Organization.

The package's parameters give the systems, profiles and extensions the maps
write. Their defaults are the Integrated Campaign Registry's (see
[examples/import/icr-dhis2/import.toml](../examples/import/icr-dhis2/import.toml)).
To write other URLs, change a parameter, for example
`import run --param base=https://example.org`.

### Loading only what changed

Compare after the import, with `import run --previous`, `--changed` and
`--removed` (see [import.md](import.md#running-an-import-repeatedly)). The
package's `import.toml` leaves a MeasureReport's `date`, the analytics build
time, out of the comparison:

```sh
fhir-pipes import run --package examples/import/icr-dhis2 --in raw --out fhir --overwrite \
  --previous fhir --changed fhir-changed --removed fhir-removed.txt
```

`--removed` lists every resource of the previous run that this run did not
write, of every type in the folder. It has no scope of indicators, org
units and periods, as `dhis2 reports --removed` has: give each run the same
`--level`, `--period` and indicator options, and the same `locations` and
`measures` selection, or the resources the run did not ask for are listed
as removed. The lines are `MeasureReport/<id>`, sorted.

## The commands that write resources

```
fhir-pipes dhis2 locations --package DIR [OPTIONS] --out LOCATIONS.ndjson
fhir-pipes dhis2 measures  --package DIR [OPTIONS] --out MEASURES.ndjson
fhir-pipes dhis2 reports   --package DIR --locations LOCATIONS.ndjson [OPTIONS] --out REPORTS.ndjson
```

Each does what the matching `dhis2 extract` command does, and runs each
record through the map of the package that reads its type, as `import run`
would. It writes the resources to one NDJSON file, in the order the records
were read, not one file per type. The resources are the same as those of
`dhis2 extract` then `import run` with the same package and options.

| Option | Meaning |
|---|---|
| `--package DIR`, `$FHIR_PIPES_DHIS2_MAPS` | The import package, such as `examples/import/icr-dhis2`. Its `maps/` folder is accepted too. `--maps DIR` is another name for it. Without a package the command stops with exit 2 |
| `--param NAME=VALUE` (repeatable) | Set a parameter of the package, overriding its default |

The package must have no `[[step]]`: each record goes through the one map
that reads its type. A package with steps is refused; run `dhis2 extract`
and `import run` with it instead.

A profile given with `--profile` is not read. The `[dhis2]` profile section
these commands read before is removed; a profile that still has it does not
load (see [profiles.md](profiles.md)).

The options of `dhis2 locations` and `dhis2 measures` are those of the
matching `dhis2 extract` command. `dhis2 reports` has the options of
`dhis2 extract reports`, except that it reads Locations instead of org unit
records, and it can compare with an earlier run:

| Option | Meaning |
|---|---|
| `--locations FILE` | NDJSON of Locations that carry the org unit UIDs: the output of `dhis2 locations`, a snapshot's `locations.ndjson`, or a `Location.ndjson` that `import run` wrote. The UID is read from the identifier under `<base>/identifiers/dhis2/<instance>/orgunit`, where `base` is the package's parameter. An org unit with no Location is skipped and reported as `unmapped_org_unit` |
| `--previous PREV.ndjson` | Compare with an earlier run. `--out` still gets every report, so it can be the baseline for the next run |
| `--changed FILE` | With `--previous`: the new and changed reports, to load |
| `--removed FILE` | With `--previous`: the ids of reports DHIS2 no longer returns, to delete from the FHIR server. Reports for indicators, levels or periods not in this run are left alone |
| `--updated-since DATE` | With `--source data-values` and `--previous`: recompute only the periods with data values changed since then |

`--previous` reads the package's parameters `base` (each report's `measure`
is `<base>/Measure/<id>`) and `periodExtension` (the extension that holds
the period) to tell which previous reports this run covers. A
MeasureReport's `date` is not compared.

```sh
fhir-pipes dhis2 reports --package examples/import/icr-dhis2 … --out bgd-reports.ndjson \
  --previous bgd-reports.ndjson --changed bgd-changed.ndjson --removed bgd-removed.txt
```

`--resume` of `dhis2 reports` is also refused when the package or a
`--param` has changed.

## Example

The Sierra Leone demo instance is open. Its address carries the DHIS2
version and changes with each release; https://play.dhis2.org lists the
current ones:

```sh
export FHIR_PIPES_DHIS2_URL=https://play.im.dhis2.org/stable-2-42-7-1
export FHIR_PIPES_DHIS2_USER=admin FHIR_PIPES_DHIS2_PASSWORD=district
fhir-pipes dhis2 extract locations --instance sl-demo --id-prefix sle --country-code SLE --out sl-raw
fhir-pipes dhis2 extract measures --instance sl-demo --indicator-group srxUNt2w5jn --out sl-raw
fhir-pipes dhis2 extract reports --instance sl-demo --indicator-group srxUNt2w5jn \
  --period 2025 --period LAST_12_MONTHS --level 2 --level 3 --out sl-raw
fhir-pipes import run --package examples/import/icr-dhis2 --in sl-raw --out sl-fhir
for t in Organization Location Measure MeasureReport; do
  fhir-pipes load --server URL --in sl-fhir/$t.ndjson
done
```

The same with the commands that write resources:

```sh
fhir-pipes dhis2 locations --package examples/import/icr-dhis2 --instance sl-demo \
  --id-prefix sle --country-code SLE --out sl-locations.ndjson
fhir-pipes dhis2 measures --package examples/import/icr-dhis2 --instance sl-demo \
  --indicator-group srxUNt2w5jn --out sl-measures.ndjson
fhir-pipes dhis2 reports --package examples/import/icr-dhis2 --instance sl-demo \
  --indicator-group srxUNt2w5jn --locations sl-locations.ndjson \
  --period 2025 --period LAST_12_MONTHS --level 2 --level 3 --out sl-reports.ndjson
for f in sl-locations sl-measures sl-reports; do fhir-pipes load --server URL --in $f.ndjson; done
```
