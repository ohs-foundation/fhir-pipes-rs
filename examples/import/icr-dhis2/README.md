# icr-dhis2: the DHIS2 import as an import package

An import package for `fhir-pipes import` (see
[docs/import.md](../../../docs/import.md)) that turns DHIS2 records into
resources of the Integrated Campaign Registry: Locations and facility
Organizations, Measures, and MeasureReports. It writes the same resources as
the built-in mapping of `fhir-pipes dhis2` with `examples/profiles/icr.toml`.

The records are what `fhir-pipes dhis2 locations|measures|reports --records`
writes (see [docs/dhis2.md](../../../docs/dhis2.md#records-and-the-import-package)).
fhir-pipes prepares them from the DHIS2 API: the ids, the parent only when it
was exported, a boundary merged from the children, counts as whole numbers,
breakdowns attached to their totals. The maps only turn one record into its
resources.

- `models/`: the logical models of the three record types: `DHIS2OrgUnit`,
  `DHIS2Indicator` and `DHIS2Value`. fhir-pipes builds the same files in
  for `dhis2 --maps`.
- `maps/`: `DHIS2OrgUnitToICR`, `DHIS2IndicatorToICR` and `DHIS2ValueToICR`,
  one per record type, and three ConceptMaps for the codes. This is the
  folder `dhis2 --maps examples/import/icr-dhis2/maps` runs.
- `import.toml`: the name and a `[compare]` section, and no steps, so each
  record file goes through the one map that reads its type. `[compare]`
  leaves a MeasureReport's `date`, when DHIS2 last built its analytics
  tables, out of `import run --previous`.
- `tests/sample/`: synthetic records and the resources expected from them.
  The org units are a country with no geometry of its own (its boundary is
  merged from its districts), two districts with boundaries, one of them
  closed, a facility with a point, a closed facility, and a facility whose
  parent was not exported, with org unit groups from a group set and from a
  group in no set. The indicators are a percent, a plain number, a data
  element and a data set's reporting rate. The values are for two months
  with two breakdowns and a campaign, and for a date range with no
  breakdown and no campaign.

```sh
fhir-pipes import check --package examples/import/icr-dhis2 --in examples/import/icr-dhis2/tests/sample/in
fhir-pipes import test  --package examples/import/icr-dhis2
fhir-pipes import run   --package examples/import/icr-dhis2 \
  --in examples/import/icr-dhis2/tests/sample/in --out /tmp/icr-dhis2-fhir
```

`tests/dhis2_import.rs` checks that `tests/sample/in/` is what the `dhis2`
commands write with `--records` for its fake DHIS2 server, and that
`tests/sample/expected/` is what the built-in mapping writes for the same
responses. To change the sample, change the scenarios there, then:

```sh
FHIR_PIPES_UPDATE_SAMPLE=1 cargo test --test dhis2_import sample
fhir-pipes import test --package examples/import/icr-dhis2 --update
cargo test --test dhis2_import
```
