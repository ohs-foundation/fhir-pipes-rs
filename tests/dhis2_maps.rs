//! The `fhir-pipes dhis2` tests again, with `--maps`: the Locations, Measures
//! and MeasureReports are written by the FHIR Mapping Language maps in
//! examples/import/icr-dhis2/maps instead of the built-in mapping. Every test in
//! tests/dhis2.rs has to pass both ways.

#[path = "dhis2.rs"]
mod suite;
