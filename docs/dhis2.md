# DHIS2 import

Reads a DHIS2 instance (Web API, 2.38 or later) and writes FHIR NDJSON for
`fhir-pipes load`. Nothing is written to DHIS2. See
[locations.md](locations.md) for the Location registry the org units become.

```
fhir-pipes dhis2 locations --dhis2 URL --instance NAME --out LOCATIONS.ndjson
fhir-pipes dhis2 measures  --dhis2 URL --instance NAME --indicator-group UID --out MEASURES.ndjson
fhir-pipes dhis2 reports   --dhis2 URL --instance NAME --indicator-group UID --locations LOCATIONS.ndjson \
                     --period LAST_12_MONTHS --level 2 --level 3 --out REPORTS.ndjson
```

`--help` lists every option.

## Common options

| Option | Meaning |
|---|---|
| `--dhis2`, `$FHIR_PIPES_DHIS2_URL` | The instance's base URL; a trailing `/api` is fine |
| `--dhis2-token`, `$FHIR_PIPES_DHIS2_TOKEN` | Personal access token, sent as `Authorization: ApiToken …` |
| `--dhis2-user` / `--dhis2-password`, `$FHIR_PIPES_DHIS2_USER` / `$FHIR_PIPES_DHIS2_PASSWORD` | Basic authentication |
| `--instance` | A short name for the instance: letters, digits and hyphens, up to 16 characters, such as `sl-demo` |
| `--profile`, `$FHIR_PIPES_PROFILE` | A profile with a `[dhis2]` section. Without one the command stops with exit 2. See [profiles.md](profiles.md) |

Use a read-only DHIS2 user. Keep
`--instance` the same across runs: it prefixes every id and tags what is
written, and ids are deterministic, so a re-run replaces the same resources.

## dhis2 locations

Turns the org unit tree into Locations. Levels above the facility level
become admin units. The facility level (`--facility-level N`, default the
deepest) becomes facility Locations, each paired with an Organization
(`managingOrganization` is `org-<location id>`). Location ids are
`PREFIX-<uid>`.

| Option | Meaning |
|---|---|
| `--id-prefix` | Id prefix. Default: the instance name |
| `--country-code` | Puts a national admin code on the top org unit; `transform locations` partitions by it (`SLE`, `NGA`) |
| `--facility-level` | The org unit level that becomes facilities |
| `--root UID` | Export one subtree |
| `--group-set UID`, `--org-unit-group UID`, `--all-org-unit-groups` | Add org unit groups to each Location's `type` |

Polygons become the boundary extension and a Point becomes `position`. An
admin unit with no geometry gets the union of its children's polygons.

## dhis2 measures

Turns indicators into Measures. Choose them with `--indicator-group` and
`--indicator` (both repeatable). `--data-element-group`, `--data-element`,
`--data-element-group-set` and `--data-set` add data elements (as `cohort`
Measures) and data set reporting rates (as `proportion` Measures). The id is
`NAME-<uid>`.

A percent indicator (factor 100) is scored `proportion`, another factor
`ratio`, and a denominator of `1` `cohort`.

## dhis2 reports

Asks `/api/analytics` for the selected indicators at each `--level` and
`--period`, and writes one `summary` MeasureReport per indicator, org unit and
period, with id `NAME-<indicator>-<orgunit>-<period>`. `--locations` is an
NDJSON of Locations (the output of `dhis2 locations`, or a snapshot's
`locations.ndjson`); an org unit with no Location is skipped and reported as
`unmapped_org_unit`.

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
| `--campaign TYPE/ID` | Adds the campaign extension, for example `CarePlan/bgd-mr-sia-2026`. Report ids do not include the campaign |
| `--source` | `analytics` (default) or `data-values` |

### Resuming and comparing with an earlier run

- `--resume` continues a failed or stopped run from `OUT.ndjson.resume`. It is
  refused if the arguments (other than `--concurrency`), the DHIS2 URL, an org
  unit count, the analytics build time, or the `--locations` or `--previous`
  file have changed. Run without `--resume` to start over.
- `--previous PREV.ndjson` compares with an earlier run. `--out` still gets
  every report, so it can be the baseline for the next run. `--changed FILE`
  gets the new and changed reports to load. `--removed FILE` gets the ids of
  reports DHIS2 no longer returns, to delete from the FHIR server. Reports
  for indicators, levels or periods not in this run are left alone.

```sh
fhir-pipes dhis2 reports … --out bgd-reports.ndjson --previous bgd-reports.ndjson \
  --changed bgd-changed.ndjson --removed bgd-removed.txt
```

### Computing values from data values

`--source data-values` reads raw data values and computes each value itself,
in the same shape as analytics. Only SUM and AVERAGE_SUM_ORG_UNIT data
elements and simple indicator expressions are supported; anything else is
refused, naming the indicator. `--updated-since DATE` (with `--previous`)
recomputes only the periods holding data values changed since then.

## Example

The Sierra Leone demo instance is open:

```sh
export FHIR_PIPES_DHIS2_URL=https://play.im.dhis2.org/stable-2-41-10
export FHIR_PIPES_DHIS2_USER=admin FHIR_PIPES_DHIS2_PASSWORD=district
fhir-pipes dhis2 locations --instance sl-demo --id-prefix sle --country-code SLE --out sl-locations.ndjson
fhir-pipes dhis2 measures --instance sl-demo --indicator-group srxUNt2w5jn --out sl-measures.ndjson
fhir-pipes dhis2 reports --instance sl-demo --indicator-group srxUNt2w5jn --locations sl-locations.ndjson \
  --period 2025 --period LAST_12_MONTHS --level 2 --level 3 --out sl-reports.ndjson
for f in sl-locations sl-measures sl-reports; do fhir-pipes load --server URL --in $f.ndjson; done
```
