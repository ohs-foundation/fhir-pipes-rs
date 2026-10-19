# fhir-pipes-sof

The [SQL on FHIR v2](https://build.fhir.org/ig/FHIR/sql-on-fhir-v2/) engine
behind `fhir-pipes views` and `fhir-pipes refresh`: it parses
`ViewDefinition` resources and turns FHIR data into flat tables, in memory or
as generated SQL.

This crate is `octofhir-sof` from
[octofhir/sof](https://github.com/octofhir/sof) by the OctoFHIR Team
(MIT OR Apache-2.0), by way of the Recode Labs fork, brought into
fhir-pipes-rs with its history and renamed `fhir-pipes-sof`. It keeps only
what fhir-pipes uses. The upstream CLI is now `fhir-pipes views` (`generate`
is `views sql`); see the [fhir-pipes README](../../README.md).

- **Two execution paths**, both passing the full official content-test suite
  (144/144):
  - a **database-free in-memory evaluator** over `serde_json`, with FHIRPath
    evaluated by [octofhir-fhirpath](https://crates.io/crates/octofhir-fhirpath),
    and
  - a **SQL generator** that lowers FHIRPath to PostgreSQL over JSONB or to
    DuckDB over JSON.
- **Writers** for the rows: CSV, NDJSON, JSON, and Parquet (the `parquet`
  feature; Snappy compression; temporal columns as strings by default or as
  native Arrow types). CSV and JSON write each value as it is. Parquet writes
  an integral number (`5.0`) in an integer column as the integer and, with
  native temporal types, a dateTime with a time in a `date` column as its
  date part as written; any other value that does not fit its column's type
  is written as null and counted per column
  (`OutputWriter::write_with_summary`).
- Support for the v2.1 additions: the `repeat` directive and the `%rowIndex`
  environment variable, on both paths.

Lint and validation of ViewDefinitions are in
[fhir-pipes-sof-lint](../sof-lint).

## Features

- `parquet`: the Parquet writer (`output::ParquetWriter`).
- `postgres`: `ViewRunner`, which runs the generated SQL against PostgreSQL.
  SQL generation needs no database driver.

## Library

```rust
use fhir_pipes_sof::output::{OutputWriter, get_writer};
use fhir_pipes_sof::{CompiledView, SqlGenerator, ViewDefinition};

let view = ViewDefinition::from_json(&json)?;

// In memory, on four threads. Rows keep the input order.
let compiled = CompiledView::compile_blocking(&view)?;
let result = compiled.execute_parallel(&resources, 4)?;
get_writer("csv")?.write(&result, &mut std::io::stdout())?;

// Or generate SQL (PostgreSQL by default; `with_dialect(Dialect::DuckDb)`).
let sql = SqlGenerator::new().generate(&view)?.sql;
```

`execute` (async), `execute_blocking` and `execute_parallel` compile and run
in one call. A compiled view can also run one resource at a time, from async
code:

```rust
let compiled = CompiledView::compile(&view).await?;
for resource in &resources {
    for row in compiled.execute_resource(resource).await? {
        // row is the column values in column order
    }
}
```

`create_table` gives the `CREATE TABLE` for a view's generated columns in the
ANSI, PostgreSQL or DuckDB dialect.

## Known limitations

The in-memory engine has no FHIR model, so it does not know an element's
declared type or whether it is a choice element. `ofType(T)` with a FHIR
datatype `T` reads the JSON name a choice element has for that type
(`value.ofType(Quantity)` reads `valueQuantity`). On an element that is not a
choice element that name does not exist, so the result is empty even when the
type is right: `name.family.ofType(string)` and `name.ofType(HumanName)` find
nothing. It never returns a value of another type, so a `where` filter using
it does not let wrong rows through. (The FHIRPath engine's own `ofType` is
not used there: without a model it compares the type with the element's name,
so `code.ofType(code)` on a CodeableConcept matches, and it fails on most
complex types.)

`ofType` with a resource type (`contained.ofType(Practitioner)`) is left to
the engine and filters by `resourceType`.

## FHIR versions

Execution does not depend on the FHIR version: the engine navigates the
resource JSON and uses no StructureDefinition, so the same view runs
unchanged over R4, R4B, R5 or R6 resources (the suite includes an R5
`CodeableReference` case).

## Conformance

The official reference tests from
[FHIR/sql-on-fhir.js](https://github.com/FHIR/sql-on-fhir.js) are vendored
under `tests/spec/`. Extra cases are in `tests/extra/` (all three paths) and
`tests/extra_memory/` (in memory only: functions the SQL generator cannot
lower). Both execution paths pass the full v2.1 suite (144/144). Run them
with `cargo test -p fhir-pipes-sof`:

- in memory: always (`tests/conformance_memory.rs`);
- DuckDB: when the `duckdb` CLI is on `PATH` (`tests/conformance_duckdb.rs`);
- PostgreSQL: with the `postgres` feature and `SOF_CONFORMANCE_DB` set to a
  database URL (`tests/conformance.rs`). The tests create a `conf` schema in
  that database.

`SOF_RESULT_JSON=<path>` makes the in-memory run write a result file in the
format of the sql-on-fhir.org registry; see [CONFORMANCE.md](CONFORMANCE.md).

### FHIRPath

`tests/fhirpath_hl7.rs` runs the official HL7 FHIRPath test suite for R4
(`tests/fhirpath/`, from FHIR/fhir-test-cases) through the in-memory evaluator,
each expression as a ViewDefinition column. fhir-pipes passes 929 of the 1,001
tests. The 72 it fails are listed, with the reason each fails, in
`tests/fhirpath/known-failures.json`, and the test fails if that list goes out
of date in either direction. Of the 72:

- 27 pass in octofhir-fhirpath when it is given the FHIR R4 model, which
  fhir-pipes does not load (it uses `EmptyModelProvider`);
- 29 fail in octofhir-fhirpath even with the model, mostly tests expecting a
  type error the engine does not raise;
- 16 come from fhir-pipes' own layer: `lowBoundary()`/`highBoundary()` with an
  out-of-range precision return a value instead of nothing; `ofType()` on an
  element that is not a choice element (`name.ofType(HumanName)`) returns
  nothing; and under `forEach` over primitive values, `$this.toString()` fails.

`tests/fhirpath/SOURCE.md` says where the data comes from and how to refresh it.

## License

MIT OR Apache-2.0.
