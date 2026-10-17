# Troubleshooting

## Exit codes

| Code | Meaning | What to do |
|---|---|---|
| 0 | success | |
| 2 | an error the user can fix before running again: a bad or missing flag, a missing or invalid config file, profile, ViewDefinition or input file, an unknown config key, `--only catalog` without `[catalog]`, a missing profile, an option the build lacks | read the message, fix the input; retrying unchanged gives the same error |
| 1 | a failure while running: network, server, database, a tool (duckdb, tippecanoe, rclone), a hook, a disk write | check the server, credentials, network, tools; the next run picks up where state allows |
| 1 | `views validate`, `views lint`, `views test` or `profile check` found problems | read the findings |

Errors go to stderr, starting with `fhir-pipes:`. Clap's own argument errors
(`error: the following required arguments were not provided`) also exit 2.

## Common messages

| Message or symptom | Cause | Fix |
|---|---|---|
| `unexpected argument '--server-kind'` (or `--token-auth`, `--scope`) | the binary is v0.5.0 or older | build from main: `cargo install --git https://github.com/ohs-foundation/fhir-pipes-rs --locked fhir-pipes` |
| `--db needs PostgreSQL support, which this fhir-pipes was built without` | a `--no-default-features` build | use a release binary or build with default features |
| `--package needs FHIR package support…` | `views lint --package` without `fhir-packages` | build with `--features fhir-packages`, or use `--shareable` only |
| `… needs a profile with a [dhis2] section …` or `… needs the system of the Location type codes …` | no profile | pass `--profile` or set `FHIR_PIPES_PROFILE` |
| HTTP 400 `Search offset exceeds maximum` on Medplum | search past `_offset` 10,000 without the kind | add `--server-kind medplum` (`kind = "medplum"` in `refresh`) |
| Aidbox pages slow or timing out | every page counts the whole result | add `--server-kind aidbox` |
| Aidbox `$export` kick-off answers 500 `storage-type not specified` | no bulk storage provider | set `BOX_FHIR_BULK_STORAGE_PROVIDER` on the Aidbox server, or use search |
| Medplum `$export` fails with 401 on the output files | `Binary/<id>` files need the token | add `--server-kind medplum`; the AccessPolicy needs `AsyncJob` and `Binary` |
| `cannot get a token from <token-url>` | wrong token URL, client id or secret, or unreachable endpoint | check `--token-url`; check the user has set `FHIR_PIPES_CLIENT_ID` and `FHIR_PIPES_CLIENT_SECRET` (without printing them); Aidbox needs `--token-auth basic` |
| a run fails with 401 part way | a fixed `--token` expired | use an OAuth client (`--client-id`), which renews its token |
| `count_mismatch` in the report | the number of resources paged differs from the server's count for the type or window | usually resources changed during the run; rerun with `--incremental`. The run itself does not fail |
| a type is downloaded in full on an incremental run | its count differed after the merge (deletions, or `$expunge` with `--hapi-db`) | expected; it keeps the file correct |
| `refresh` stops before running: a tool is missing | the tiles step needs duckdb and tippecanoe; push needs rclone | install them, set `[tools]`, or `--skip tiles` |
| `refresh` exits 2 naming a ViewDefinition | the view does not pass the structure checks | `fhir-pipes views validate <file>` shows the rule and place |
| `<N> values did not fit <type> and were written as null` | view column types do not match the data | fix the view's `type`; the run does not fail |
| `--hapi-db` refuses the database, naming tables or columns | not a HAPI JPA schema fhir-pipes knows, or wrong `search_path` | check the connection string; add `options=-c%20search_path%3D<schema>` |

## Checking a run without side effects

- `fhir-pipes refresh --check`: settings and tools; no network writes.
- `fhir-pipes load … --dry-run`, `fhir-pipes sync … --dry-run`: the plan, nothing sent.
- `fhir-pipes refresh --only push --dry-run`: what pushing the existing
  warehouse would change, with no other step run. `refresh --push --dry-run`
  is different: it runs the whole refresh, writing tables, the database and
  running hooks, and only the upload is a dry run.
- `fhir-pipes views validate VIEW`, `views run VIEW --input sample.ndjson`: views offline.

To check that a secret is set without showing it:
`[ -n "$FHIR_PIPES_CLIENT_SECRET" ] && echo set || echo missing`.
