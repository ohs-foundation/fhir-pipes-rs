# SQL-on-FHIR v2 conformance

`octofhir-sof` runs the official [FHIR/sql-on-fhir.js][upstream] v2.1 content
test suite (vendored at `crates/octofhir-sof/tests/spec/`, refresh with
`just update-spec-tests`). Both execution paths — the database-free in-memory
evaluator and the generated PostgreSQL — pass the full suite:

```
TOTAL  144/144
```

- In-memory: `just conformance`
- PostgreSQL: `just conformance-pg` (needs a running Postgres; see the recipe)

## Producing a result file for the registry

`sql-on-fhir.org` aggregates a [public registry][impls] of implementations.
Each entry points at a `testResultsUrl` whose document is an object keyed by
test-file name, each holding a `tests` array of
`{ name, result: { passed, reason? } }` — exactly the shape produced by the
upstream runner and consumed by the report site. (Compare the Medplum and
Safhire result files linked from the registry.)

Generate our result file in that format:

```sh
just conformance-report                 # writes conformance-results.json
just conformance-report out=results.json # custom path
```

Internally this runs the in-memory harness with `SOF_RESULT_JSON=<path>` set;
the harness serializes every outcome (title → pass/fail, plus a `reason` on
failure) into the registry shape.

## Submitting our entry

1. Host the produced JSON at a stable public URL (e.g. a GitHub raw URL on a
   release tag, or GitHub Pages).
2. Open a PR against [FHIR/sql-on-fhir.js][upstream] adding a row to
   `test_report/public/implementations.json`:

   ```json
   {
     "name": "OctoFHIR SoF",
     "url": "https://github.com/octofhir/sof",
     "description": "Pure-Rust SQL-on-FHIR v2 engine: in-memory evaluation over JSON and multi-dialect SQL generation.",
     "testResultsUrl": "https://raw.githubusercontent.com/octofhir/sof/<tag>/conformance-results.json"
   }
   ```

The report site fetches each `testResultsUrl` and renders the pass/fail matrix.

[upstream]: https://github.com/FHIR/sql-on-fhir.js
[impls]: https://github.com/FHIR/sql-on-fhir.js/blob/master/test_report/public/implementations.json
