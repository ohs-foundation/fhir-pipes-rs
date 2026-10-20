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

## FHIR model

The in-memory engine knows the type of each FHIR element from a table built
into the crate (`src/fhir_types.rs`), generated from the R4 and R5 core
StructureDefinitions by `examples/generate_fhir_types.rs`; the example's
header says how to regenerate it. No FHIR package is loaded and nothing is
fetched at run time. With the types, `is`, `as` and `ofType()` work on FHIR
types: `Patient.active.is(boolean)`, `Patient.gender.ofType(code)`,
`name.ofType(HumanName)`.

The table is the union of R4 and R5, so a view runs unchanged on either:

- an element in one version only is in the table;
- an element whose type differs between the versions (`Account.description`
  is a `string` in R4 and `markdown` in R5; 310 such elements) has no type,
  and the engine navigates it as it would without a model;
- elements the table does not have, such as those R6 adds, are navigated
  the same way.

Quantity elements (`Quantity`, `Age`, `Duration` and the rest) also have no
type. The engine turns a typed Quantity into a value that keeps only its
value, unit, code and system, and octofhir-fhirpath 0.4.56 also loses the code
and system there (octofhir/fhirpath-rs#183). Untyped, a Quantity stays the
resource's JSON, so `comparator`, `id` and extensions can still be read.

A date, dateTime, instant or time element keeps its FHIR type name, so
`issued.is(instant)` is true, but the engine holds it as a string. As a
DateTime the engine would write it back in its own form (`…05Z` as
`…05+00:00`); comparisons and boundaries read the string as a date.

## Known limitations

`ofType(T)` with a FHIR datatype `T` on a choice element reads the JSON name
the element has for that type (`value.ofType(Quantity)` reads
`valueQuantity`): the engine cannot map `value` to its JSON names itself.
Whether an element is a choice element is decided from the path. A type name
before it (`Observation.value`), or the view's resource for the first step,
says exactly; deeper in a path any choice element of that name counts, so
`contact.name.ofType(HumanName)` reads `contact.nameHumanName`, a name that
does not exist, and finds nothing (`name[x]` is a choice element of
`ImplementationGuide.definition.page`).

`ofType` with a resource type (`contained.ofType(Practitioner)`) is left to
the engine and filters by `resourceType`.

## FHIR versions

The engine navigates the resource JSON, and its FHIR model is the union of R4
and R5 (see above), so the same view runs unchanged over R4, R4B, R5 or R6
resources (the suite includes an R5 `CodeableReference` case).

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
each expression as a ViewDefinition column. fhir-pipes passes 962 of the 1,001
tests. The 39 it fails are listed, with the reason each fails, in
`tests/fhirpath/known-failures.json`, and the test fails if that list goes out
of date in either direction. Of the 39:

- 22 expect a type error before evaluation, which needs a type checker. One of
  them, `testPolymorphismAsB`, passed without the FHIR model only because the
  expression failed; kotlin-fhirpath records it as a test the specification
  does not support;
- 16 are octofhir-fhirpath bugs or gaps. Fixes for 9 of them are open or
  unreleased upstream (octofhir/fhirpath-rs#176, #177, #179): `is`/`as`
  precedence, `hasValue()` on complex types, and a date-only dateTime. With
  them, one test that passes today only because of the precedence bug
  (`isGuardAndBad`, which expects a type error) fails, so the total goes to
  970. The other 7 are Quantity unit arithmetic (1), `conformsTo()` (3),
  `as()` with a date or time type (1), `as()` over several items, which
  octofhir allows on purpose for FHIR's own invariants (1), and
  `@1973-12-25T00:00:00.000+10:00 + 0.1 's'`, which other engines fail too (1);
- 1 is fhir-pipes' own: `1.toDecimal().lowBoundary()`. A decimal's boundary
  uses the digits of its JSON form, because a resource's `1.0` reaches the
  engine as `1`; the engine also holds `1.toDecimal()` as `1.0`. With
  octofhir/fhirpath-rs#182 the engine keeps a decimal's digits and this can
  go.

`tests/fhirpath/SOURCE.md` says where the data comes from and how to refresh it.

## License

MIT OR Apache-2.0.
