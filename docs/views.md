# Running ViewDefinitions directly

`fhir-pipes views` works with SQL-on-FHIR ViewDefinitions without a warehouse: run them over local FHIR files or PostgreSQL, generate their SQL, and check them. `refresh` ([refresh.md](refresh.md)) uses the same engine to build a warehouse.

The engine is in [crates/sof](../crates/sof), brought in from [octofhir/sof](https://github.com/octofhir/sof). It evaluates views in memory on every CPU, reads each resource type once however many views use it, and passes the official SQL-on-FHIR v2.1 test suite (144 of 144 cases). [crates/sof-lint](../crates/sof-lint) checks each view's structure before anything runs.

| Command | What it does |
|---|---|
| `views run` | Run one view or a directory of views over `--input` (files) or `--db` (PostgreSQL). Output is CSV (the default), NDJSON, JSON or Parquet (`--output`). |
| `views sql` | Print the SQL for a view (`--dialect duckdb` or `postgres`), or a `CREATE TABLE` for its columns (`--ddl`). |
| `views validate` | Check each view's structure. |
| `views lint` | Check each selector against the FHIR R4 and R5 types. |
| `views test` | Run SQL-on-FHIR test cases. |

`fhir-pipes views <command> --help` lists every option.

```sh
fhir-pipes views run patient_flat.json --input raw/Patient.ndjson          # CSV to stdout
fhir-pipes views run views/ --input raw/ --output parquet --out tables/   # one file per view
fhir-pipes views sql patient_flat.json --dialect duckdb
fhir-pipes views validate patient_flat.json --sarif > validate.sarif        # for CI code scanning
fhir-pipes views lint patient_flat.json --shareable                         # also the portable FHIRPath subset
fhir-pipes views test crates/sof/tests/spec
```

## Input

- `--input` is a file or a directory (read with its subdirectories, hidden files skipped). It reads `.json` and `.ndjson` files, plain or gzip. Bundles are read too; a `urn:uuid` reference to another entry is rewritten to that entry's `Type/id`, so `getReferenceKey()` and joins work.
- A directory that holds both `X.ndjson` and `X.ndjson.gz` is refused (exit 2), since every row would appear twice.
- NDJSON is read 20,000 lines at a time and resources no view reads are dropped as they are read.
- `views run --db URL` (or `FHIR_PIPES_DATABASE_URL`) runs against PostgreSQL holding FHIR resources as JSONB. It needs the `postgres` feature, which the release binaries have.

## Output

- `--out FILE.gz` writes CSV, NDJSON and JSON as gzip. Parquet with a `.gz` name is refused.
- `--output parquet` writes typed columns. A value that does not fit its column type is written as null (an integral `5.0` in an `integer` column is written as `5`; `5.5` is not rounded). Each case is counted per column and printed on stderr at the end; the run does not fail. CSV and NDJSON write every value as it is.
- With `--parquet-temporal native`, a dateTime in a `date` column is written as its date part as written (not converted to UTC), and partial dates (`2026`, `2026-06`) are floored to the start of the period.

## Checks

- `views validate` and `views lint` exit 1 when they find problems. Both write findings as JSON (`--json`) or SARIF (`--sarif`).
- `views lint` reports unknown elements (FH01), a choice element without `ofType()` (FH02), and a complex element, an array or a Reference in a scalar column (FH03 to FH05). `--shareable` also checks the portable FHIRPath subset; `--allow-fn NAME` exempts an engine-specific function.
- `views lint --package hl7.fhir.r4.core --version 4.0.1` checks against a FHIR package instead (the local cache `~/.fhir/packages`, else packages.fhir.org). `--package` also takes a package.tgz, an unpacked package or a directory of StructureDefinitions. It needs a build with the `fhir-packages` feature (see the [README](../README.md#building-and-testing)).
