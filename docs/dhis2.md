# DHIS2 import

Reads a DHIS2 instance (Web API, 2.38 or later) and writes what it reads as
records, which an import package turns into FHIR NDJSON for
`fhir-pipes load`. Nothing is written to DHIS2. See
[locations.md](locations.md) for the Location registry the org units become.

```
fhir-pipes dhis2 extract all --config dhis2.toml --out RAW
fhir-pipes import run --package examples/import/icr-dhis2 --in RAW --out FHIR
```

`dhis2.toml` is the instance's settings file (see
[The settings file](#the-settings-file)). `extract all` runs
`extract locations`, `extract measures` and `extract reports`, which can
also be run one at a time, with the settings as options or in the file.

`dhis2 extract` writes one file of records per command in the folder `RAW`.
`import run` maps the folder with the package
[examples/import/icr-dhis2](../examples/import/icr-dhis2) and writes one
NDJSON file per resource type.

The DHIS2 commands are a separate program, `fhir-pipes-dhis2`, the DHIS2
connector of [connectors.md](connectors.md). fhir-pipes runs it as
`fhir-pipes dhis2 …`, which is how the examples here call it.
`fhir-pipes-dhis2 …` with the same arguments does the same.

`dhis2 locations`, `dhis2 measures` and `dhis2 reports` do both steps in one
command, for one record type, and write one NDJSON file of resources. They
need a package; see
[The commands that write resources](#the-commands-that-write-resources).

`--help` lists every option.

## Installing fhir-pipes-dhis2

Download `fhir-pipes-dhis2-<target>` (`.exe` on Windows) from the same
[release](https://github.com/ohs-foundation/fhir-pipes-rs/releases) as
fhir-pipes; `SHA256SUMS` covers both. Rename it to `fhir-pipes-dhis2`
(`fhir-pipes-dhis2.exe` on Windows) and put it in the folder of
`fhir-pipes` or in a folder on the `PATH`:

```sh
curl -LO https://github.com/ohs-foundation/fhir-pipes-rs/releases/latest/download/fhir-pipes-dhis2-aarch64-apple-darwin
chmod +x fhir-pipes-dhis2-aarch64-apple-darwin
mv fhir-pipes-dhis2-aarch64-apple-darwin /usr/local/bin/fhir-pipes-dhis2
```

Or build it with Rust:
`cargo install --git https://github.com/ohs-foundation/fhir-pipes-rs --locked fhir-pipes-dhis2`.

Without it, `fhir-pipes dhis2` stops with exit 2: `unrecognized subcommand
'dhis2'`, then `fhir-pipes-dhis2 was not found next to fhir-pipes or on the
PATH`.

## Common options

| Option | Meaning |
|---|---|
| `--dhis2`, `$FHIR_PIPES_DHIS2_URL` | The instance's base URL; a trailing `/api` is fine |
| `--dhis2-token`, `$FHIR_PIPES_DHIS2_TOKEN` | Personal access token, sent as `Authorization: ApiToken …` |
| `--dhis2-user` / `--dhis2-password`, `$FHIR_PIPES_DHIS2_USER` / `$FHIR_PIPES_DHIS2_PASSWORD` | Basic authentication |
| `--instance` | A short name for the instance: letters, digits and hyphens, up to 16 characters, such as `sl-demo` |
| `--report FILE` | Also write the run report as JSON |
| `--config FILE`, `$FHIR_PIPES_DHIS2_CONFIG` | The instance's settings file; see [The settings file](#the-settings-file) |

Use a read-only DHIS2 user. Keep `--instance` the same across runs: it
prefixes every id and is in every record, and ids are deterministic, so a
re-run replaces the same resources.

No `dhis2` command reads the profile, whether it is given as `--profile`
before or after `dhis2`, or as `$FHIR_PIPES_PROFILE`. The systems, profiles and extensions
the resources carry are parameters of the package (see
[import.md](import.md#parameters)).

## The settings file

The settings of one DHIS2 instance (its server, its instance name, and what
to export) can be written down once in a TOML file and given with
`--config FILE` or `$FHIR_PIPES_DHIS2_CONFIG`. Every command reads it. Each
key stands for an option, and the option can still be given:

```toml
url = "https://play.im.dhis2.org/stable-2-42-7-1"
instance = "sl-demo"

[locations]
id_prefix = "sle"
country_code = "SLE"

[indicators]
indicator_groups = ["srxUNt2w5jn"]

[reports]
levels = [2, 3]
periods = ["2025", "LAST_12_MONTHS"]
```

[examples/dhis2/sierra-leone.toml](../examples/dhis2/sierra-leone.toml) is
a commented file for the open Sierra Leone demo.

| Key | Option | Meaning |
|---|---|---|
| `url` | `--dhis2`, `$FHIR_PIPES_DHIS2_URL` | The instance's base URL |
| `instance` | `--instance` | The short name of the instance |
| `user` | `--dhis2-user`, `$FHIR_PIPES_DHIS2_USER` | User name for Basic authentication; the password comes from `$FHIR_PIPES_DHIS2_PASSWORD` |
| `retries` | `--retries` | Attempts per request |
| `timeout` | `--timeout` | Seconds per request |
| `package` | `--package`, `$FHIR_PIPES_DHIS2_MAPS` | The import package of `dhis2 locations\|measures\|reports`, relative to the file's folder. `dhis2 extract` does not use it |
| `[parameters]` `NAME = VALUE` | `--param NAME=VALUE` | A parameter of that package: a string, a number or `true`/`false` |
| `[locations]` `id_prefix` | `--id-prefix` | Location id prefix |
| `[locations]` `country_code` | `--country-code` | National admin code of the top org unit |
| `[locations]` `facility_level` | `--facility-level` | Org unit level of the facilities |
| `[locations]` `root` | `--root` of `extract locations` | Read one subtree |
| `[locations]` `group_sets` | `--group-set` | Org unit group set UIDs |
| `[locations]` `org_unit_groups` | `--org-unit-group` | Org unit group UIDs |
| `[locations]` `all_org_unit_groups` | `--all-org-unit-groups` | `true`: every org unit group |
| `[indicators]` `indicator_groups` | `--indicator-group` | Indicator group UIDs |
| `[indicators]` `indicators` | `--indicator` | Indicator UIDs |
| `[indicators]` `data_element_groups` | `--data-element-group` | Data element group UIDs |
| `[indicators]` `data_elements` | `--data-element` | Data element UIDs |
| `[indicators]` `data_element_group_sets` | `--data-element-group-set` | Data element group set UIDs |
| `[indicators]` `data_sets` | `--data-set` | Data set UIDs, for their reporting rates |
| `[reports]` `periods` | `--period` | DHIS2 periods |
| `[reports]` `ranges` | `--range` | Date ranges, `START:END` |
| `[reports]` `levels` | `--level` | Org unit levels to report |
| `[reports]` `root` | `--root` of `extract reports` | Only org units in this tree |
| `[reports]` `breakdowns` | `--breakdown` | Dimensions to break values down by |
| `[reports]` `campaign` | `--campaign` | Campaign reference, `TYPE/ID` |
| `[reports]` `source` | `--source` | `analytics` or `data-values` |
| `[reports]` `chunk` | `--chunk` | Indicators per analytics request |
| `[reports]` `max_rows` | `--max-rows` | Rows to aim for per analytics request |
| `[reports]` `concurrency` | `--concurrency` | Analytics requests in flight at once |
| `[reports]` `max_analytics_age` | `--max-analytics-age` | Refuse analytics older than this, such as `7d` |

`[indicators]` is what both `measures` and `reports` select. A value in
the file is checked as the option checks it, and a file with an error stops
every command with exit 2, naming the file and the key. So does a key the
file does not know. The file is read the way `fhir-pipes.toml` is
([refresh.md](refresh.md#settings)).

### Which value is used

- An option given on the command line, or its environment variable, wins
  over the file's key.
- A repeatable option replaces the file's whole list; the two are not
  merged. `--level 4` with `levels = [2, 3]` in the file reports level 4
  only.
- Some options together form one setting, and any of them on the command
  line replaces all of the file's keys for it: `--period` and `--range`
  replace `periods` and `ranges`; any of the six indicator options
  replaces the whole `[indicators]` table; `--org-unit-group` or
  `--all-org-unit-groups` replaces `org_unit_groups` and
  `all_org_unit_groups`.
- `--param NAME=VALUE` replaces the file's value of that one parameter; the
  file's other parameters still apply.
- A token (`--dhis2-token`, `$FHIR_PIPES_DHIS2_TOKEN`) or a user name given
  on the command line or in the environment is used instead of the file's
  `user`.

A run still needs `url`, `instance`, an indicator selection (for measures
and reports), and for reports `levels` and `periods` or `ranges`. When
neither the command line nor the file gives one, the command stops with
exit 2 and says which option or key to give.

These are per run, and only options: `--out`, `--resume`, `--report`,
`--previous`, `--changed`, `--removed`, `--updated-since`, `--org-units`
and `--locations`. `--report` records the settings file's path under
`config`.

### Passwords and tokens

The file holds no secret. A key whose name has `password`, `token` or
`secret` in it is refused, with exit 2: give the password in
`$FHIR_PIPES_DHIS2_PASSWORD` and the personal access token in
`$FHIR_PIPES_DHIS2_TOKEN` (or with `--dhis2-password` and `--dhis2-token`).
The user name may be in the file as `user`, as `fhir-pipes.toml` allows
`server.username`. Package parameters are not checked for these names.

### dhis2 extract all

```
fhir-pipes dhis2 extract all [--config FILE] [OPTIONS] --out DIR
```

Runs `extract locations`, then `extract measures`, then `extract reports`,
into `DIR`, as the three commands would. It works without a file when the
options give every setting. The first step that fails stops the run, with
that step's exit code, and the error names the step. The settings of every
step are checked before the first one runs. A step whose settings are
missing, such as reports with no levels, stops the run before it starts;
`--skip reports` (repeatable, also `locations` or `measures`) leaves a step
out on purpose.

It takes the options of the three commands. `--root` is the root of both
the locations and the reports step; in the file each has its own `root`.
`--report FILE` writes one JSON object with the settings file's path and
the report of each step. `--resume` applies to the reports step: a
locations or measures step whose record file is already in `DIR` is not
run again, since rewriting the org unit records would change the plan the
reports step resumes.

### Checking a settings file

```
fhir-pipes dhis2 config check --config FILE [--connect]
```

Reads and checks the file without contacting the server, and prints the
settings a run with no options would use, as TOML: the file's keys, with
`url`, `user` and `package` from their environment variables when those are
set, and the package path resolved. Comment lines above say where the
credentials come from, never their values, and whether each step of
`extract all` has its settings. Exits 0 when the file is valid, 2 when it
is not.

`--connect` also signs in to the server, which checks the URL and the
credentials. The UIDs in the file are not checked against the server.

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
fhir-pipes-dhis2 prepares each record from the DHIS2 API: the ids, the parent only
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
from has changed. A setting counts whether it was given as an option or in
the settings file: changing `chunk` in the file is refused like changing
`--chunk`, and moving a value from the file to the command line is not a
change. A run of `dhis2 extract reports` is not resumed by
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
current ones. [examples/dhis2/sierra-leone.toml](../examples/dhis2/sierra-leone.toml)
has its settings:

```sh
export FHIR_PIPES_DHIS2_USER=admin FHIR_PIPES_DHIS2_PASSWORD=district
fhir-pipes dhis2 extract all --config examples/dhis2/sierra-leone.toml --out sl-raw
fhir-pipes import run --package examples/import/icr-dhis2 --in sl-raw --out sl-fhir
for t in Organization Location Measure MeasureReport; do
  fhir-pipes load --server URL --in sl-fhir/$t.ndjson
done
```

The same with the commands that write resources, which take the file's
`package`:

```sh
f=examples/dhis2/sierra-leone.toml
fhir-pipes dhis2 locations --config $f --out sl-locations.ndjson
fhir-pipes dhis2 measures --config $f --out sl-measures.ndjson
fhir-pipes dhis2 reports --config $f --locations sl-locations.ndjson --out sl-reports.ndjson
for f in sl-locations sl-measures sl-reports; do fhir-pipes load --server URL --in $f.ndjson; done
```
