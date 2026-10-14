# DHIS2 import

Related: [docs/locations.md](locations.md) for the Location registry the
imported org units become, and `load` for sending the NDJSON to a FHIR server.

```
fhir-pipes dhis2 locations --dhis2 URL --instance NAME --out LOCATIONS.ndjson [--id-prefix P] [--country-code CC]
fhir-pipes dhis2 measures  --dhis2 URL --instance NAME --indicator-group UID --out MEASURES.ndjson
fhir-pipes dhis2 reports   --dhis2 URL --instance NAME --indicator-group UID --locations LOCATIONS.ndjson
                     --period LAST_12_MONTHS --level 2 --level 3 --out REPORTS.ndjson
```

Reads a DHIS2 instance (Web API, 2.38 or later) and writes FHIR NDJSON for
`fhir-pipes load`. Nothing is written to DHIS2. Sign in with a personal access
token (`--dhis2-token`, `$FHIR_PIPES_DHIS2_TOKEN`, sent as `Authorization:
ApiToken …`) or a username and password (`--dhis2-user` / `--dhis2-password`,
`$FHIR_PIPES_DHIS2_USER` / `$FHIR_PIPES_DHIS2_PASSWORD`, Basic). `--dhis2` (or
`$FHIR_PIPES_DHIS2_URL`) is the instance's base URL; a trailing `/api` is fine.
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
fhir-pipes models facilities everywhere else. Location ids are `PREFIX-<uid>`
(`--id-prefix`, default the instance name). Each Location carries the org
unit UID as its `official` identifier, plus the DHIS2 code when there is
one. Polygon and MultiPolygon geometry becomes the boundary extension, a
Point becomes `position`. An admin unit DHIS2 has no geometry for (often
the country itself) gets the union of its children's polygons, reported as
`boundary_from_children`, so it still has a boundary and a row in the
dataset. A closed org unit is `inactive`. `--country-code`
puts a national admin code on the top org unit, which is what `transform`
partitions by (`SLE`, `NGA`, …). `--root UID` exports one subtree.
`--group-set UID` (repeatable) adds each org unit's group in an org unit
group set, such as Facility Type, to its Location's `type` (and its
Organization's, for facilities), coded the same way as a `--breakdown` on
that set, so a facility's type lines up with the Facility Type strata.
`--org-unit-group UID` (repeatable) does the same for single org unit
groups, such as the group of all districts DHIS2 dashboards list places
by (`OU_GROUP-<uid>`); `--all-org-unit-groups` takes every group. A group
that belongs to group sets is coded exactly as `--group-set` codes it (a
coding per set, system `…/dimension/<set uid>`), so the same group looks
the same however it was asked for, and is added once. A group in no set
is coded under `…/CodeSystem/dhis2/NAME/org-unit-group`, code the group
uid.

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
for that org unit and period, then divides. fhir-pipes never sums or averages
indicator values; ask for each level you want. Requests use
`includeNumDen` and `skipRounding`, and send `Cache-Control: no-cache` so a
proxy cannot answer from before the last analytics run. Periods can be
ISO (`2025`, `2025Q1`, `202501`, `2025W5`, `2025April`, …) or relative
(`LAST_12_MONTHS`, `THIS_YEAR`, …); a period with no data returns no rows
and so no reports. Values reflect the last analytics table run, which fhir-pipes
prints at the start, with a warning when it is more than 7 days old.
`--max-analytics-age 7d` (also `36h`, `2w`) refuses the run instead when
the build is older than that, naming the build time and its age. DHIS2
gives the build time in its own time zone; fhir-pipes reads it as UTC, which is
a few hours either way at most.

`--range START:END` (repeatable, ISO dates, both included, e.g.
`2026-04-05:2026-05-20`) asks for one value per indicator and org unit
over the whole range, such as a campaign's days, using DHIS2's `startDate`
and `endDate` parameters instead of a period. It can be used with or
instead of `--period`. DHIS2 computes the value over the range the way it
does for a period. The report's `period.start` and `period.end` are the
two dates, the `dhis2-period` extension reads `2026-04-05/2026-05-20` (an
ISO 8601 interval), and the id ends in `20260405-20260520`. That makes ids
17 characters longer than a monthly report's; fhir-pipes refuses a range whose
ids would go over FHIR's 64 characters (a reporting rate with an
`--instance` longer than 7 characters) before asking for any values. Each
range is its own request (split by indicators if DHIS2 calls it too large),
and breakdowns work the same.

Every report carries where its figures come from, in the
`https://icr.healthcampaigns.org/StructureDefinition/dhis2-source`
extension (after `dhis2-period` and the campaign): `valueCode`
`analytics` or `data-values`. Reports written before this extension
existed lack it, so a `--previous` comparison against one counts every
report as changed, once.

`--source data-values` reads raw data values (`/api/dataValueSets`,
`orgUnit` with `children=true`, `startDate`/`endDate`) instead of
analytics, and fhir-pipes computes each value itself, in the same shape: same
ids, periods, populations, scores and strata. Each batch's values are
added up the org unit tree of the `--locations` file (`partOf`) to every
requested level at once, so the Locations must reach down to where data
is entered. The rules followed, and where they stop:

- A data element is its total over category option combos and attribute
  option combos, over the period and every org unit below. Aggregation
  type SUM adds everything; AVERAGE_SUM_ORG_UNIT averages each org unit's
  values over the period (per category option combo), then adds the org
  units. Any other aggregation type is refused.
- An indicator's numerator and denominator are evaluated from its
  expressions: `#{de}`, `#{de.coc}`, `#{de.coc.aoc}`, `#{de.*.aoc}`,
  `C{constant}`, numbers, `+ - * /` and parentheses. A missing item
  counts as 0 unless every item in the expression is missing, which gives
  no value (DHIS2's rule for indicators; they have no
  `missingValueStrategy` setting). The value is numerator × factor ÷
  denominator, × days in the year ÷ days in the period when the indicator
  is annualised; no value when the denominator is missing or 0.
  Expressions using `R{…}` (reporting rates), `I{…}` (program
  indicators), `D{…}`, `A{…}`, `OUG{…}`, `[days]` or functions such as
  `if()` are refused, naming the indicator; so is `--data-set` (reporting
  rates from completeDataSetRegistrations are not computed).
- A data value counts towards a period when its own period lies wholly
  inside it; one that runs across the period's edge (a week across two
  months) counts towards neither and is reported as `straddling_period`.
  Periods must be days, ISO weeks, months, bi-months, quarters,
  six-months or years, or a `--range`; relative periods are refused.
- `--breakdown` must be a category: each category option combo counts
  towards that category's option it holds. An item whose data elements do
  not all have the category gets no breakdown, as in analytics. Org unit
  group set and data element group set breakdowns are not computed.
- `date` is when the run started (fhir-pipes computed the values then), not the
  analytics build; `--max-analytics-age` does not apply.

`--updated-since DATE` (with `--source data-values` and `--previous`) asks
DHIS2 which data values changed or were deleted since then
(`lastUpdated`, `includeDeleted`) and recomputes only the periods and
ranges holding them, every indicator in them. `--out` then gets those
reports and keeps the previous ones for every other period, so it stays
the whole baseline for the next run; `--changed` has what to load. A
daily run:

```sh
fhir-pipes dhis2 reports … --source data-values --out bgd-reports.ndjson \
  --previous bgd-reports.ndjson --updated-since 2026-10-12 \
  --changed bgd-changed.ndjson --removed bgd-removed.txt
```

`--campaign TYPE/ID` (e.g. `CarePlan/bgd-mr-sia-2026`) adds the ICR
campaign extension to every report written,
`{"url": "https://icr.healthcampaigns.org/StructureDefinition/campaign",
"valueReference": {"reference": "CarePlan/bgd-mr-sia-2026"}}`, after the
`dhis2-period` extension. Report ids do not include the campaign, so a
report for the same indicator, org unit and period from another run
replaces it whatever campaign that run named: one run should cover one
campaign's periods and ranges.

`--root UID` asks for each level as `LEVEL-n;UID`, only the org units in
that one's tree. Use it when the DHIS2 user can see top-level org units
outside the tree being reported on; DHIS2 otherwise refuses the whole
request (E7120).

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
as `unmapped_org_unit`.

Large exports (hundreds of periods, thousands of org units) are split into
batches. Before asking for values, fhir-pipes asks DHIS2 how many org units each
`--level` has (under `--root` when given). Each analytics request carries
up to `--chunk` indicators (10) and as many periods as keep indicators ×
periods × org units under `--max-rows` (20,000 rows; DHIS2's own limit is
50,000 by default). Only periods of the same kind share a request: days
with days (`20250101`), weeks with weeks (`2025W5`), months with months,
and so on. A relative period (`LAST_12_MONTHS`) always goes alone, since
DHIS2 expands it into several periods. A batch's breakdowns are asked for
with the same periods; they return one row per option, so they are not
counted in the estimate. When DHIS2 answers that a result is over its row
limit (E7128, E7151), fhir-pipes splits the request in two and tries again: the
periods first, then the indicators.

Reports are written as each batch arrives, so memory holds a few batches
at a time rather than the whole export. `--concurrency` (4) batches are
fetched at once; the output is written in planned order (level, then
indicator chunk, then periods, sorted by id within each batch), so it is
the same file whatever the setting. After each batch fhir-pipes prints a line
like
`[37/412] level 4 · 20260421…20260427 (7 periods) · 10 items: 33,656 values · 4m12s elapsed, ~38m left`.

The file is written as `OUT.ndjson.tmp` and renamed when the run succeeds.
After each batch fhir-pipes syncs it to disk and records in `OUT.ndjson.resume`
how many batches it holds and how long it is. A run that fails, or is
stopped, keeps both files; `--resume` carries on after the last batch
recorded, cutting off anything written after it (a batch half-written
when the run stopped is fetched again), and the output is the same file
an uninterrupted run writes, with the same summary. fhir-pipes refuses to resume
if anything that decides the batches or their content has changed since:
the arguments (other than `--concurrency`), the DHIS2 URL, an org unit
count, when DHIS2 last built its analytics tables, or the `--locations` or
`--previous` file (size and modification time). Run without `--resume` to
start over. `OUT.ndjson.resume` is removed when the run succeeds. A gzip
`--out` (`OUT.ndjson.gz`) is written as one gzip member per batch: members
one after another make one valid gzip file, and the end of a member is a
place the file can be cut back to, so resuming works the same.

`--previous PREV.ndjson` compares this run with an earlier one, so a rerun
need only send what changed to the FHIR server. `--out` still gets every
report the run returns: it is the baseline for the next run, and may be
the same file as `--previous` (the previous file is read before the new
one replaces it). The comparison counts each report as new (its id is not
in PREV), changed or unchanged, and prints
`Compared with PREV: 12 new, 340 changed, 822,580 unchanged, 3 removed`.

- `--changed CHANGED.ndjson` gets the new and changed reports, in the same
  order as `--out`: load this one.
- `--removed REMOVED.txt` gets, one per line, the ids of reports in PREV
  that this run asked for again but DHIS2 no longer returns (a value
  deleted in DHIS2), so they can be deleted from the FHIR server
  (`DELETE MeasureReport/<id>`). A report counts as asked for again when
  its Measure is one of this run's indicators, its Location is one of the
  org units DHIS2 lists for this run's levels, and its period is one of
  this run's periods (relative periods expanded the way DHIS2 lists them)
  or ranges. Reports for other indicators, levels or periods are left
  alone, so a run that covers part of an export never lists the rest as
  removed.

Content is compared through a hash of each report. `date` is left out: it
is when DHIS2 last built its analytics tables, which changes every night
whether or not a value did, so a report whose value is the same keeps the
`date` of the run that last sent it. `meta.lastUpdated` and
`meta.versionId` (set by a FHIR server) are left out too. Memory holds a
hash per id, not the previous reports, and the previous file is read a
second time at the end to find the removed ids.

```sh
fhir-pipes dhis2 reports … --out bgd-reports.ndjson                # first run: load all of it
fhir-pipes dhis2 reports … --out bgd-reports.ndjson --previous bgd-reports.ndjson \
  --changed bgd-changed.ndjson --removed bgd-removed.txt   # later runs: load the changes
```

Report kinds for `--source data-values`: `straddling_period`,
`unsupported_period` (a data value whose period type has no dates here),
`non_numeric_value`, and `unmapped_org_unit` for a data value at an org
unit with no Location.

Report kinds: `stratum_without_total` (a breakdown value whose total DHIS2
did not return), `fractional_count` (a numerator or denominator that is not a
whole number, rounded because FHIR counts are integers; common for
averages and annualised denominators), `bad_number`, `unmapped_org_unit`,
`period_without_dates`, `invalid_id`; for `locations`, `bad_geometry`,
`parent_not_exported`, `below_facility_level` and `boundary_from_children`.

The Sierra Leone demo instance is open, so the whole chain can be tried:

```sh
export FHIR_PIPES_DHIS2_URL=https://play.im.dhis2.org/stable-2-41-10
export FHIR_PIPES_DHIS2_USER=admin FHIR_PIPES_DHIS2_PASSWORD=district
fhir-pipes dhis2 locations --instance sl-demo --id-prefix sle --country-code SLE --out sl-locations.ndjson
fhir-pipes dhis2 measures --instance sl-demo --indicator-group srxUNt2w5jn --out sl-measures.ndjson
fhir-pipes dhis2 reports --instance sl-demo --indicator-group srxUNt2w5jn --locations sl-locations.ndjson \
  --period 2025 --period LAST_12_MONTHS --level 2 --level 3 --out sl-reports.ndjson
for f in sl-locations sl-measures sl-reports; do fhir-pipes load --server URL --in $f.ndjson; done
```
