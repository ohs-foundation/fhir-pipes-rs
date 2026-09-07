# octofhir/sof

A Rust toolkit for [SQL on FHIR v2](https://build.fhir.org/ig/FHIR/sql-on-fhir-v2/):
parse `ViewDefinition` resources and turn FHIR data into flat tables — with no
database required, or by generating PostgreSQL.

**Documentation:** <https://octofhir.github.io/sof>

- **Two interchangeable execution paths**, both passing the full official
  content-test suite (144/144):
  - a **database-free in-memory evaluator** over `serde_json`, and
  - a **PostgreSQL generator** that lowers FHIRPath to SQL over JSONB.
- **Writers**: CSV, NDJSON, JSON, and Parquet (feature-gated; temporal columns
  as strings by default or native Arrow types with `--parquet-temporal native`).
- **Offline lint & validation** of ViewDefinitions (FH01–FH10) with rustc-style
  diagnostics, plus JSON and SARIF output for CI.
- Full support for the v2.1 additions: the `repeat` directive and the
  `%rowIndex` environment variable, on both paths.

## Install

```sh
# Prebuilt binary (detects your platform, verifies SHA-256):
curl -fsSL https://raw.githubusercontent.com/octofhir/sof/main/install.sh | sh

# From crates.io:
cargo install octofhir-sof-cli

# …or from a clone of this repo:
cargo install --path crates/octofhir-sof-cli

# …or build and run in place:
cargo build --release
./target/release/octofhir-sof --help
```

Prebuilt binaries for macOS, Linux and Windows are attached to each
[GitHub release](https://github.com/octofhir/sof/releases).

## Usage

```sh
# Run a view over FHIR files, no database — CSV to stdout
octofhir-sof run view.json --input patients.ndjson

# Stream a large NDJSON export with bounded memory (NDJSON in, NDJSON out)
cat export/*.ndjson | octofhir-sof run view.json --input - --output ndjson

# Run every view in a directory in one pass, one output file per view
octofhir-sof run views/ --input data/ --output csv --out out/

# Parquet with native temporal types (Date32 / Timestamp(µs, UTC) / Time64)
# instead of the spec-default strings; partial dates floor to period start
octofhir-sof run view.json --input data.ndjson --output parquet \
  --parquet-temporal native --out view.parquet

# Generate SQL for the view (PostgreSQL JSONB by default, or DuckDB JSON)
octofhir-sof generate view.json
octofhir-sof generate view.json --dialect duckdb

# Generate a CREATE TABLE for the view's columns (ansi | postgres | duckdb)
octofhir-sof generate view.json --ddl --dialect postgres

# Run against PostgreSQL instead of files
octofhir-sof run view.json --db postgres://localhost/fhir --output csv

# Validate a ViewDefinition offline (no FHIR package needed)
octofhir-sof validate view.json
octofhir-sof validate view.json --sarif > results.sarif

# Lint selectors and generated SQL against a FHIR package (any version)
octofhir-sof lint view.json --package hl7.fhir.r4.core
octofhir-sof lint view.json --package hl7.fhir.r5.core --version 5.0.0

# Check a view against the portable ShareableViewDefinition FHIRPath subset
# (offline, no package). --allow-fn exempts a registered custom function.
octofhir-sof lint view.json --shareable

# Run the official content-test format in memory
octofhir-sof test tests/
```

Exit codes: `0` on success, `1` on validation errors, lint errors, test
failures, or I/O errors.

## Library

```rust
use octofhir_sof::{ViewDefinition, execute, SqlGenerator};

let view = ViewDefinition::parse(&json_text)?;

// In-memory:
let result = execute(&view, &resources)?;

// Or generate SQL:
let sql = SqlGenerator::new().generate(&view)?.sql;
```

For bounded-memory streaming, compile once and run resource-at-a-time:

```rust
use octofhir_sof::CompiledView;

let compiled = CompiledView::compile(&view)?;
for resource in resources {
    for row in compiled.execute_resource(&resource)? {
        // row is the column values in column order
    }
}
```

## SQL dialects

`generate` emits a PostgreSQL JSONB query by default and a DuckDB JSON query
with `--dialect duckdb`; both come from one dialect-parametrized generator and
pass the full content-test suite (DuckDB verified through the `duckdb` CLI). The
in-memory engine (`run --input`, `execute`) needs no database at all.

## FHIR versions

Execution is **version-agnostic**: the engine navigates the resource JSON and is
not coupled to any StructureDefinition, so the same view runs unchanged over R4,
R4B, R5 or R6 resources (the suite includes an R5 `CodeableReference` case). The
schema-driven lint (`lint --package`) accepts any package the canonical manager
can resolve — pass the package name and `--version`, e.g. `hl7.fhir.r4.core`,
`hl7.fhir.r4b.core`, `hl7.fhir.r5.core`, or an R6 build — there is no hard-coded
version.

## Conformance

The official reference tests from
[FHIR/sql-on-fhir.js](https://github.com/FHIR/sql-on-fhir.js) are vendored under
`crates/octofhir-sof/tests/spec/` (refresh with `just update-spec-tests`). Both
execution paths pass the full v2.1 suite (144/144). Run them:

```sh
just conformance                 # in-memory (no database)
just conformance-pg              # against PostgreSQL (needs Docker)
just conformance-duckdb          # against DuckDB (needs the `duckdb` CLI)
just conformance-report          # emit a result file for the sql-on-fhir.org registry
```

See [CONFORMANCE.md](CONFORMANCE.md) for the result-file format and how to submit
our entry to the implementation registry.

## License

MIT OR Apache-2.0.
