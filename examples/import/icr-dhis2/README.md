# icr-dhis2: the DHIS2 import as an import package

An import package for `fhir-pipes import` (see
[docs/import.md](../../../docs/import.md)) that turns DHIS2 records into
resources of the Integrated Campaign Registry: Locations and facility
Organizations, Measures, and MeasureReports. It writes the same resources as
the mapping that was built into `fhir-pipes dhis2` until 12 November 2026,
with `examples/profiles/icr.toml`.

The records are what `fhir-pipes dhis2 extract locations|measures|reports`
writes (see [docs/dhis2.md](../../../docs/dhis2.md#the-records)).
`fhir-pipes dhis2 locations|measures|reports --package
examples/import/icr-dhis2` run the same maps one record at a time and write
the resources in one step.
fhir-pipes prepares them from the DHIS2 API: the ids, the parent only when it
was exported, a boundary merged from the children, counts as whole numbers,
breakdowns attached to their totals. The maps only turn one record into its
resources.

`dhis2 extract reports` finds each org unit in the `DHIS2OrgUnit` records
that `dhis2 extract locations` wrote, so the three extract runs can write to
one folder and one `import run` maps it. A `DHIS2Value` record's
`locationId` is `<idPrefix>-<uid>` of the org unit's record, the id
`DHIS2OrgUnitToICR` gives its Location. A package that gives Locations
other ids has to write the same ids in `DHIS2ValueToICR`'s `subject`.

```sh
fhir-pipes dhis2 extract locations --instance sl-demo --id-prefix sle --out raw
fhir-pipes dhis2 extract measures  --instance sl-demo --indicator-group srxUNt2w5jn --out raw
fhir-pipes dhis2 extract reports   --instance sl-demo --indicator-group srxUNt2w5jn \
  --period 2025 --level 2 --out raw
fhir-pipes import run --package examples/import/icr-dhis2 --in raw --out fhir
```

- `models/`: the logical models of the three record types: `DHIS2OrgUnit`,
  `DHIS2Indicator` and `DHIS2Value`. fhir-pipes builds the same files in
  for `map run`.
- `maps/`: `DHIS2OrgUnitToICR`, `DHIS2IndicatorToICR` and `DHIS2ValueToICR`,
  one per record type, and three ConceptMaps for the codes.
- `import.toml`: the name, the parameters, a `[compare]` section, and no
  steps, so each record file goes through the one map that reads its type.
  The parameters are the systems, profiles and extensions the maps write
  (`base`, `projectTagSystem`, `locationProfile`, `periodExtension` and the
  rest), with the values the `[dhis2]` section of
  `examples/profiles/icr.toml` gave the built-in mapping as defaults;
  `--param base=https://example.org` on `import run` or on the `dhis2`
  commands changes one for a run (see
  [Parameters](../../../docs/import.md#parameters)). The records carry none
  of these values. `dhis2 reports` reads `base` and `periodExtension` too,
  to find Locations by their org unit identifier and to compare with
  `--previous`. `[compare]` leaves a MeasureReport's `date`, when DHIS2
  last built its analytics tables, out of `import run --previous`.
- `tests/`: test cases for `import test`, each synthetic records (`in/`) and
  the resources expected from them (`expected/`). The expected files are
  what the built-in mapping wrote for these records before it was removed;
  they are not regenerated from the maps.
- `tests/sample/`: the case of `tests/dhis2_import.rs`.
  The org units are a country with no geometry of its own (its boundary is
  merged from its districts), two districts with boundaries, one of them
  closed, a facility with a point, a closed facility, and a facility whose
  parent was not exported, with org unit groups from a group set and from a
  group in no set. The indicators are a percent, a plain number, a data
  element and a data set's reporting rate. The values are for two months
  with two breakdowns and a campaign, and for a date range with no
  breakdown and no campaign.
- `tests/org-units/`, `tests/indicators/`, `tests/values/`: the cases of the
  unit tests in `src/dhis2/locations.rs`, `measures.rs` and
  `reports/build.rs`: a closed facility with no code, groups in two sets,
  a geometry that is neither a boundary nor a point, a ratio, an item with
  nothing but an id, counts that are not whole numbers, breakdown options
  with no name.
- `tests/data-values/`: values computed from raw data values
  (`--source data-values`) on three levels.
- `tests/batches/`: a run split into many analytics requests, as
  `--resume` continues it.

```sh
fhir-pipes import check --package examples/import/icr-dhis2 --in examples/import/icr-dhis2/tests/sample/in
fhir-pipes import test  --package examples/import/icr-dhis2
fhir-pipes import run   --package examples/import/icr-dhis2 \
  --in examples/import/icr-dhis2/tests/sample/in --out /tmp/icr-dhis2-fhir
```

The tests check that each case's `in/` is what fhir-pipes prepares for its
fake DHIS2 server or its unit test, and that `dhis2 locations|measures|
reports --package` and `import run` write the case's `expected/`. After a
change in how fhir-pipes prepares records, rewrite the `in/` files with:

```sh
FHIR_PIPES_UPDATE_SAMPLE=1 cargo test --test dhis2_import
FHIR_PIPES_UPDATE_SAMPLE=1 cargo test --bin fhir-pipes the_records_are_the_package_case
cargo test
```

A change in `expected/` is a change in the resources the package writes:
make it in the maps, and change the expected files by hand or with
`fhir-pipes import test --update`, and check the difference.
