# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **A FHIR model for the in-memory engine** — the FHIRPath engine now knows
  each element's type, from a table of the R4 and R5 core types generated into
  the crate (`src/fhir_types.rs`, by `examples/generate_fhir_types.rs`), in
  place of `EmptyModelProvider`. `is`, `as` and `ofType()` work on FHIR types
  (`Patient.active.is(boolean)`, `Patient.gender.ofType(code)`), and the HL7
  FHIRPath suite goes from 936 to 962 of 1,001. Elements whose type differs
  between R4 and R5, and Quantity elements, stay untyped; dates keep the form
  they were written in. Output of the 22 views tried on Synthea data is
  unchanged; they run about 15% slower.
- **Native temporal types in Parquet output** — `ParquetWriter::with_temporal(
  TemporalEncoding::Native)` (CLI: `run --output parquet --parquet-temporal
  native`) writes `date` columns as Arrow `Date32`, `dateTime`/`instant` as
  `Timestamp(Microsecond, UTC)` and `time` as `Time64(Microsecond)`, so DuckDB,
  ClickHouse, Spark and pandas see real temporal types without a cast. The
  default (`TemporalEncoding::Text`) is unchanged and keeps the FHIR string
  representation per the spec's ANSI type mapping. In native mode,
  reduced-precision FHIR values are floored to the start of the period they
  name (`2026` → `2026-01-01`, `2026-03` → `2026-03-01`, a date-only
  `dateTime` → midnight UTC); offsets are normalized to UTC; unparseable
  values are written as null.
- **`OutputWriter::write_with_summary` and `WriteSummary`** — the Parquet
  writer returns, per column, the values it did not write as they are,
  instead of logging them at `warn` (which nothing showed). CSV and JSON
  return an empty summary.
- **`ViewDefinition::output_columns`** — the view's output columns in order,
  calling back for each `unionAll` whose branches do not have the same
  column names. The in-memory evaluator and `fhir-pipes-sof-lint`'s
  `validate_structure` both use it, in place of a walk of their own; their
  errors and findings are unchanged.

### Changed

- **`ofType()` follows the path through the FHIR model** — whether an element
  is a choice element (so `value.ofType(Quantity)` reads `valueQuantity`) is
  now decided for the type or backbone element the path reaches it from,
  including under `forEach`, so `contact.name.ofType(HumanName)` on a Patient
  finds the name. `forEach` items keep their type for the paths under them.
  The hand-made `choice_elements` table is gone; the generated type table
  replaces it.
- **octofhir-fhirpath 0.4.57** — with the `is`/`as` precedence, `hasValue()`,
  boundary and date-only dateTime fixes from octofhir/fhirpath-rs#176–#179;
  the HL7 FHIRPath suite goes from 962 to 970 of 1,001.
- **Parquet: values that do not match their column's type.** An integral
  number (`5.0`) in an `integer` or `integer64` column is written as the
  integer instead of null; `5.5` is still null. With
  `TemporalEncoding::Native`, a `dateTime` with a time in a `date` column
  (`2026-06-15T23:30:00-05:00`) is written as its date part as written
  (`2026-06-15`, not converted to UTC) instead of null. Anything else that
  cannot be converted is still null, and is now counted in the
  `WriteSummary`.

### Removed

API that fhir-pipes does not use. Nothing the `fhir-pipes` binary does
changes.

- The async writers: `output::AsyncOutputWriter` and its `write` and
  `write_streaming` for every format, `output::AsyncWriter` and
  `output::OutputFormat`. `OutputWriter::content_type` and
  `OutputWriter::file_extension`.
- Writer options: `ParquetCompression` and `ParquetWriter::with_compression`
  (Parquet is written with Snappy, the default before);
  `CsvWriter::with_header`, `with_delimiter` and `with_quote` (CSV has a
  header, commas and double quotes); the `pretty` option of `NdjsonWriter`
  and `JsonArrayWriter`. The writers are now unit structs.
- `SqlExecutor` and `run_with`; `ViewRunner::with_generator`, `explain` and
  `pool`.
- `ViewResult::row_count` (use `data.len()`), `is_empty`, `row_as_object`,
  `column_values` and `column_count`; `GeneratedSql::ctes`, which was always
  empty.
- `ColumnInfo::nullable`, `description`, `with_nullable` and
  `with_description`; `ColumnType::null_representation`.
- `ViewDefinition::parse` (use `from_json`) and `column_names`;
  `Constant::value`.
- `SqlGenerator::with_table_pattern` and `with_row_filter`.
- `CompiledView::resource_type`.
- The `futures-util` dependency and the `tokio-test` dev-dependency.

### Fixed

- **In-memory FHIRPath cases the view engine got wrong** (found by probes
  outside the official suite):
  - Constant substitution tokenizes the path, so a `%` inside a string
    literal, a backtick-quoted name or a comment is no longer taken for a
    constant (`name.where(family = '50%off')`). This also applies to the SQL
    generator. In `run`/`execute`, the environment variables `%resource`,
    `%rootResource`, `%context`, `%ucum`, `%sct`, `%loinc`, `` %`vs-name` ``
    and `` %`ext-name` ``, and names bound by `defineVariable()` in the same
    path, are passed to the engine instead of failing as undefined
    constants. The SQL generator still rejects them.
  - `ofType(T)` is joined onto the element name (`value.ofType(Quantity)` →
    `valueQuantity`) only when `T` is a FHIR datatype, and not inside string
    literals. `contained.ofType(Practitioner)` is left to the engine and
    filters by resource type. On an element that is not a choice element,
    `ofType` with a datatype still finds nothing; see the README's known
    limitations.
  - `lowBoundary()` / `highBoundary()` accept any expression as their base,
    such as `(1.5 + 1).lowBoundary()`.
  - Inside `forEach`, `forEachOrNull` and `repeat`, the resource stays the
    root: `%resource` is the resource, and `getReferenceKey()` resolves a `#`
    reference against the resource's `contained`. `$this` and `%context`
    are still the element, also inside a lambda such as `where()`.

## [0.1.2]

### Added

- **`SqlExecutor` trait and `run_with`** — execute a ViewDefinition by generating
  SQL and delegating row fetching to an external database layer, instead of the
  bundled sqlx `ViewRunner`. Host applications can reuse their own instrumented
  Postgres pool while keeping SoF's SQL generation and `ViewResult` shape.

### Changed

- Bumped sqlx (`sqlx-core`, `sqlx-postgres`) to `0.9` for the Postgres
  `ViewRunner` path.

## [0.1.1]

### Changed

- **In-memory execution now supports the full FHIRPath function set.** `run` and
  `execute` delegate per-expression evaluation to the `octofhir-fhirpath`
  engine, so functions beyond the SQL-on-FHIR essentials — `substring`, `upper`,
  `lower`, `length`, math, etc. — work in memory. The library stays
  network-free (no FHIR ModelProvider; `ofType` is rewritten on the path).
  SoF semantics (forEach/unionAll/repeat, `%rowIndex`, scalar-vs-collection,
  contained-resource keys) are preserved.
- The SQL generation path (`generate`, PostgreSQL/DuckDB) remains a hand-lowered
  subset and is unchanged: `run` is now intentionally richer than `generate`,
  which errors clearly on a function it cannot lower to SQL.
- `execute` and `CompiledView::{compile, execute_resource}` are now `async`; a
  sync `execute_blocking` is provided for non-async callers.

### Fixed

- `lint` no longer runs the generated SQL through the bundled SQL analyzer,
  which produced false positives (Rf01/Jb01) against the generator's
  lateral-join aliases. Findings are reported at the spec level only.

## [0.1.0]

First release. A pure-Rust [SQL-on-FHIR v2](https://build.fhir.org/ig/FHIR/sql-on-fhir-v2/)
toolkit: a network-free library plus a thin CLI.

### Library (`octofhir-sof`)

- **In-memory evaluator** — `execute(view, &[Value])` and a streaming
  `CompiledView`, evaluating ViewDefinitions directly over `serde_json` with no
  database.
- **Multi-dialect SQL generation** — `SqlGenerator` emits PostgreSQL (JSONB) or,
  with `with_dialect(Dialect::DuckDb)`, DuckDB (JSON) for the SELECT query, plus
  ANSI/PostgreSQL/DuckDB `CREATE TABLE` DDL.
- Full FHIRPath collection model: `forEach`/`forEachOrNull`, nested `select`,
  `unionAll`, `where`, `constant`, `%rowIndex`, `repeat`, `ofType`, boundary
  functions, and `getResourceKey`/`getReferenceKey` — including resolving
  **contained resources** (a `#id` reference keys into the resource's
  `contained[]`).
- **Version-agnostic execution** across FHIR R4/R4B/R5/R6 (navigates JSON, no
  StructureDefinition coupling).

### Lint (`octofhir-sof-lint`)

- Structural rules FH06–FH10 (SQL-safe names, one iteration construct per
  select, duplicate columns, `unionAll` shape, empty view).
- Schema-driven selector rules FH01–FH05 against a FHIR package (unknown
  element with did-you-mean, un-narrowed choice, complex/array/Reference into a
  scalar column).
- **FH11** — the ShareableViewDefinition FHIRPath allow-list (opt-in).

### CLI (`octofhir-sof`)

- `generate` (`--dialect postgres|duckdb`, `--ddl`), `run` (files/stdin/dir/`--db`,
  CSV/NDJSON/JSON/Parquet, bounded-memory streaming), `validate`, `test`, and
  `lint` (`--package`/`--version`, `--shareable`, `--allow-fn`) with
  rustc-style diagnostics, `--json` and `--sarif` output.

### Conformance

- Passes the full official v2.1 content-test suite **144/144** on both the
  in-memory and PostgreSQL paths, and on DuckDB through the `duckdb` CLI. See
  [CONFORMANCE.md](CONFORMANCE.md).

[0.1.1]: https://github.com/octofhir/sof/releases/tag/v0.1.1
[0.1.0]: https://github.com/octofhir/sof/releases/tag/v0.1.0
