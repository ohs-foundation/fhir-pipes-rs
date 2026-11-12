# digit-visits: an example import package

A small import package for `fhir-pipes import` (see
[docs/import.md](../../../docs/import.md)). The data is synthetic: four
DIGIT tasks, three beneficiaries, three household members and two
facilities, made up for the tests.

It shows every part of a package:

- `models/`: logical models of five DIGIT record types: `DIGITFacility`,
  `DIGITTask`, `DIGITProjectBeneficiary`, `DIGITHouseholdMember`, and
  `DIGITVisit`, the shape of a row of the `DigitVisit` Library.
- `maps/`: `DIGITFacilityToICR` maps each facility to a Location on its
  own. Three staging maps turn tasks, beneficiaries and household members
  into Task and Basic resources for the views. `DIGITVisitToICR` maps each
  visit to a Task and one MedicationAdministration per person. Two
  ConceptMaps translate the DIGIT codes.
- `views/`: three ViewDefinitions that flatten the staged resources. This
  is the standard SQL on FHIR form: each view's `resource` is a FHIR
  resource type. A package can instead let views read the record files
  directly, with no staging map; see
  [Record views](../../../docs/import.md#record-views).
- `libraries/`: `DigitVisit`, a SQLQuery that joins the views and groups the
  tasks of one round, household and worker into visits, split where more
  than `visitGapSeconds` (2 hours by default) pass between tasks. Its
  columns are named as the elements of `DIGITVisit`.
- `import.toml`: the steps in order, a data test on the visit key and two
  required columns, a SQL test that no task is lost in the joins, and three
  parameters (see [Parameters](../../../docs/import.md#parameters)):
  `idPrefix` (`dgt-`), which the maps put before the ids they write;
  `stagingTag` (`digit-staging`), the tag the staging maps write and the
  `StgTask` view selects on; and `visitGapSeconds` (7200), which the
  `DigitVisit` Library declares and reads as `:visitGapSeconds`.
- `tests/sample/`: the records, and the resources expected from them.
  Household h1 has a morning visit (one person treated, one refusal) and a
  separate afternoon revisit; household h2 has one visit, with nobody at
  home: status `failed` and a treated count of 0.

```sh
fhir-pipes import check --package examples/import/digit-visits --in examples/import/digit-visits/tests/sample/in
fhir-pipes import test  --package examples/import/digit-visits
fhir-pipes import run   --package examples/import/digit-visits \
  --in examples/import/digit-visits/tests/sample/in --out /tmp/digit-fhir
# The same with visits split after 1 hour, and another id prefix.
fhir-pipes import run   --package examples/import/digit-visits \
  --in examples/import/digit-visits/tests/sample/in --out /tmp/digit-fhir-1h \
  --param visitGapSeconds=3600 --param idPrefix=dgt2-
```

The library step needs the `duckdb` command.
