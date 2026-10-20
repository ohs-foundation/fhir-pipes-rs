# HL7 FHIRPath test suite (R4)

The official FHIRPath tests, run by `tests/fhirpath_hl7.rs`.

- `tests-fhir-r4.xml`: <https://github.com/FHIR/fhir-test-cases>,
  `r4/fhirpath/tests-fhir-r4.xml`, commit
  `a35e57c5adfe3e15ccdfb0c7a441279ace0a75b4` (2026-10-16), unchanged.
- `input/`: the resources the tests name, from `r4/` at the same commit.
  The four JSON files are copied as they are. The seven XML files are
  converted to JSON with the HL7 validator (`validator_cli.jar` 7.0.2):

  ```
  java -jar validator_cli.jar convert -version=4.0.1 -tx=n/a -output=input/<name>.json <name>.xml
  ```

  The converted `patient-example.json` equals the R4 core example
  `Patient-example.json` (hl7.fhir.r4.examples#4.0.1) apart from `text` and
  `meta`.
- `known-failures.json`: the tests fhir-pipes fails today, each with the
  reason the run gave. Written by the test itself; see `tests/fhirpath_hl7.rs`.

To refresh, replace `tests-fhir-r4.xml` and `input/` from a newer commit, then
run `FHIRPATH_UPDATE_KNOWN_FAILURES=1 cargo test -p fhir-pipes-sof --test fhirpath_hl7`
and review the change to `known-failures.json`.
