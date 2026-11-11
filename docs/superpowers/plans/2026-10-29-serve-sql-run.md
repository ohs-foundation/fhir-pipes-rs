# Approved SQLQueries over HTTP (`$sql-run` in serve) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `fhir-pipes serve` answers `$sql-run` for approved SQLQuery Libraries, by canonical, over the local warehouse or a remote copy, behind a query token of its own.

**Architecture:** A new `src/serve/query/` module: `config` parses `[serve.query]`, `catalog` holds the warehouse settings and the approved queries, `run` composes a Library and runs it with the `duckdb` command into a temp directory, and `http` holds the `/fhir` routes. `stage.rs` gains a publish generation so a local query can tell it overlapped a publish. `serve/mod.rs` wires it in: per-path tokens, startup checks, catalog reload after each run.

**Tech Stack:** Rust, axum 0.8 (already used by serve), the external `duckdb` CLI (as the refresh uses it), serde_json.

**Spec:** `docs/superpowers/specs/2026-10-29-serve-sql-run-design.md`

## Global Constraints

- Only SQLQuery Libraries are ever run. SQLViews and ViewDefinitions are never subjects (403).
- Supported `$sql-run` parameters: `subjectCanonical` (required), `_format` (`ndjson` default, `json`, `csv`), `_limit`. Every other parameter: `400` with an `OperationOutcome`.
- `_format` takes precedence over `Accept`; with neither, `ndjson`.
- Media types: `application/x-ndjson`, `application/json`, `text/csv`. `json` is one array of row objects; `csv` has a header row.
- Status codes: 400 bad request, 401 no or wrong token, 403 not approved or not a SQLQuery, 404 not in the catalog (or no `[serve.query]`), 422 the SQL fails, 503 + `Retry-After` (publish overlap twice, concurrency full, input table missing), 500 otherwise. Errors are `OperationOutcome` JSON (`application/fhir+json`).
- `[serve.query]` keys and defaults: `approve = "active"` (`"active"|"listed"|"all"`), `queries = []`, `source = "local"`, `duckdb_setup` (path, relative to the config file), `timeout = 60` (s), `max_rows = 100000`, `concurrency = 4`. `deny_unknown_fields`.
- Tokens: `FHIR_PIPES_QUERY_TOKEN` is accepted on `/fhir/*` only; `FHIR_PIPES_SERVE_TOKEN` is accepted everywhere. With `[serve.query]`, a non-loopback `--listen` needs `FHIR_PIPES_QUERY_TOKEN`.
- Truncation header: `X-Fhir-Pipes-Truncated: max_rows=<n>`, only when `max_rows` (not the caller's `_limit`) cut the result.
- Inputs are read as the refresh reads them: `read_parquet(<root>/parquet/<table>/**/*.parquet, hive_partitioning = true, union_by_name = true, hive_types_autocast = false)`; SQLViews inlined; the built-in LocationHierarchy is `location_hierarchy`; a SQLQuery input is its result table.
- duckdb is always run with `-init /dev/null`, and with `DUCKDB_AGENT_MODE=0` in tests.
- Tests that need duckdb skip (with a printed note) when it is not on PATH, as `tests/refresh_libraries.rs::have_duckdb` does.
- Unit tests run with `cargo test --bin fhir-pipes`; TMPDIR=/Volumes/maximus/tmp/fhir-pipes locally.

## Review Focus

1. A query that overlaps a local publish must not return mixed data: pinned by Task 1's generation test and Task 3's `a_publish_during_the_query_runs_it_again`.
2. A SQL error must not leak secrets from `duckdb_setup` (a `CREATE SECRET` key) into the 422 message: Task 3 `setup_secrets_are_redacted_from_errors`.
3. A query token must not reach `/run`, `/runs`, `/status`, `/metrics`: Task 5 `the_query_token_only_opens_fhir`.
4. A result exactly `max_rows` long is not reported as truncated; one longer is: Task 3 `truncation_is_reported_only_past_max_rows`.
5. A canonical with `|version`, and one with a version that does not exist (404, not 500): Task 4 `a_versioned_canonical_resolves_and_an_unknown_version_is_404`.

---

### Task 1: Publish generation in `stage.rs`

**Files:**
- Modify: `src/warehouse/stage.rs`

**Interfaces:**
- Produces: `pub fn publish_state(data: &Path) -> PublishState` and `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct PublishState { pub publishing: bool, pub generation: u64 }` in `crate::warehouse::stage`. Make the module reachable from `serve` (it is `pub(crate)` or `pub` as needed).
- The generation lives in `<data>/.publish-generation` (decimal text; absent = 0). `swap_in` writes `generation + 1` with `crate::fs::atomic_write` after the last rename and before removing `.staging/.publishing`. `publishing` is whether `<data>/.staging/.publishing` exists.

- [ ] **Step 1: Write the failing tests** (in `stage.rs`'s test module)

```rust
#[test]
fn a_publish_bumps_the_generation() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    assert_eq!(publish_state(data), PublishState { publishing: false, generation: 0 });
    let s = stage(data);
    table(&s.table("a"), "new a");
    s.publish(&mut false).unwrap();
    assert_eq!(publish_state(data), PublishState { publishing: false, generation: 1 });
}

#[test]
fn a_publish_under_way_is_seen() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".staging")).unwrap();
    fs::write(dir.path().join(".staging/.publishing"), "").unwrap();
    assert!(publish_state(dir.path()).publishing);
}
```

- [ ] **Step 2:** `cargo test --bin fhir-pipes stage::tests::a_publish` → FAIL (not defined).
- [ ] **Step 3:** Implement as in Interfaces. An unreadable or unparsable generation file reads as 0.
- [ ] **Step 4:** Same command → PASS; then `cargo test --bin fhir-pipes stage::` → all pass.
- [ ] **Step 5:** Commit `stage: count publishes in .publish-generation`.

---

### Task 2: `[serve.query]` config

**Files:**
- Create: `src/serve/query/mod.rs`, `src/serve/query/config.rs`
- Modify: `src/serve/config.rs` (add `query: Option<toml::Table>` to `ServeFile`, parsed by `query::config::parse`; `ServeConfig.query: Option<QueryConfig>`), `src/serve/mod.rs` (`pub mod query;`)

**Interfaces:**
- Produces, in `crate::serve::query::config`:
  - `pub enum Approve { Active, Listed(Vec<String>), All }`
  - `pub enum Source { Local, Remote(String) }` (`Remote` holds the prefix with any trailing `/` removed)
  - `pub struct QueryConfig { pub approve: Approve, pub source: Source, pub duckdb_setup: Option<PathBuf>, pub timeout: Duration, pub max_rows: usize, pub concurrency: usize }`
  - `pub fn parse(section: toml::Table, base: &Path) -> anyhow::Result<QueryConfig>`; `base` is the config file's directory, for `duckdb_setup`.
- `serve::config::parse` gains a `base: &Path` parameter; its one caller in `serve/mod.rs::run` passes the config file's parent.

- [ ] **Step 1: Failing tests** in `query/config.rs`:
  - `the_defaults_are_the_specs`: empty table → `Approve::Active`, `Source::Local`, no setup, 60 s, 100000, 4.
  - `listed_needs_queries_and_queries_need_listed`: `approve = "listed"` with no `queries` → error containing `queries`; `queries = ["x"]` with `approve = "active"` → error containing `approve = "listed"`.
  - `a_remote_source_and_its_setup`: `source = "r2://icr/uga/"`, `duckdb_setup = "q.sql"` → `Source::Remote("r2://icr/uga")`, setup `base.join("q.sql")`.
  - `bad_values_are_refused`: `approve = "some"`, `timeout = 0`, `max_rows = 0`, `concurrency = 0`, an unknown key → each an error.
  - In `serve/config.rs`: `a_query_section_is_parsed` (`[query]` present → `Some`), and no section → `None`.
- [ ] **Step 2:** `cargo test --bin fhir-pipes serve::` → the new tests FAIL.
- [ ] **Step 3:** Implement. Keep the `deny_unknown_fields` style of `serve/config.rs`.
- [ ] **Step 4:** Same command → PASS.
- [ ] **Step 5:** Commit `serve: parse [serve.query]`.

---

### Task 3: Running a query

**Files:**
- Create: `src/serve/query/run.rs`
- Test: in-module tests (need duckdb; skip without it)

**Interfaces:**
- Consumes: `QueryConfig`, `Source` (Task 2); `stage::publish_state` (Task 1); `library::compose::{compose, Bind}`, `library::resolve::{Catalog, Artifact, HIERARCHY_URL, HIERARCHY_TABLE}`, `library::{Library, Kind}`, `warehouse::sql::sql_path`, `redact::sql_strings`, `stop::register`.
- Produces, in `crate::serve::query::run`:
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Format { Ndjson, Json, Csv }` with `pub fn media_type(self) -> &'static str`.
  - `pub struct Output { pub body: Vec<u8>, pub rows: usize, pub truncated_at: Option<usize> }`
  - `pub enum QueryError { Unavailable(String), Sql(String), Busy, Internal(String) }` (Busy → 503 concurrency; Unavailable → 503 publish/missing table; Sql → 422).
  - `pub struct Runner { duckdb: String, root: Root, setup: Option<String>, strip_prefix: Vec<String>, timeout: Duration, max_rows: usize, slots: Semaphore-like counter }` where `enum Root { Local(PathBuf), Remote(String) }`.
  - `pub fn Runner::new(cfg: &QueryConfig, data: &Path, duckdb: &str, strip_prefix: &[String]) -> anyhow::Result<Runner>` (reads `duckdb_setup` once).
  - `pub fn Runner::run(&self, catalog: &Catalog, lib: &Library, format: Format, limit: Option<usize>) -> Result<Output, QueryError>`

**How `run` works** (the decisions; write the body):
1. Take a slot (`concurrency` at once, an `AtomicUsize` with compare-and-swap is enough); none free → `Busy`.
2. Local root: if any input table's directory (`<data>/parquet/<table>`, through SQLViews, as `libraries_step::check_inputs` walks them) is missing → `Unavailable("… has no table <t> yet")`. Remote root: a duckdb error containing `No files found` → `Unavailable`.
3. Compose for `duckdb` with a bind that returns `Bind::Relation(read_parquet(<root>/parquet/<table>/**/*.parquet …))`, `None` for SQLViews, `location_hierarchy` for `HIERARCHY_URL` (copy the logic of `library::library_sql`'s bind; factor it into a shared `pub fn` in `library/mod.rs` taking the root as a string so both use it).
4. `cap = min(limit.unwrap_or(usize::MAX), max_rows)`. In a fresh temp dir, run one duckdb script (`-init /dev/null -c <script>`, with `setup` SQL first):
   ```sql
   CREATE TEMP TABLE r AS SELECT * FROM (<sql>) AS q LIMIT <cap + 1>;
   COPY (SELECT count(*) FROM r) TO '<tmp>/n.csv' (HEADER false);
   COPY (SELECT * FROM r LIMIT <cap>) TO '<tmp>/out' (<FORMAT json | FORMAT json, ARRAY true | FORMAT csv, HEADER true>);
   ```
   DuckDB keeps insertion order, so the Library's `ORDER BY` holds.
5. Spawn with stdout/stderr piped, `stop::register` the pid, kill it after `timeout` → `Sql("timed out after N s")`. Non-zero exit → `Sql(stderr)`, with `redact::sql_strings(stderr, setup)` applied.
6. `rows = min(n, cap)`; `truncated_at = Some(max_rows)` iff `n > cap && cap == max_rows`.
7. Local root only: read `publish_state` before step 4 and after step 5. If either read has `publishing` or the generations differ, run steps 4–5 once more; if that also overlaps, `Unavailable("a refresh is publishing; retry")`.

- [ ] **Step 1: Failing tests.** A helper builds a warehouse in a temp dir: `parquet/task/country=UGA/part-0.parquet` with columns `id, n` (written with duckdb `COPY`), a ViewDefinition JSON for table `task`, a SQLView `TaskBig` (`SELECT * FROM task WHERE n > 1`) and a SQLQuery `Q` (`SELECT id, n FROM big ORDER BY n DESC`), loaded with `Catalog::load`.
  - `runs_through_a_sqlview_in_order`: ndjson body has the rows of `n > 1`, highest first.
  - `formats`: `Json` body parses as an array of the same rows; `Csv` body's first line is `id,n`.
  - `truncation_is_reported_only_past_max_rows`: with 3 matching rows, `max_rows = 3` → `truncated_at == None`; `max_rows = 2` → `Some(2)`, 2 rows; `max_rows = 2, limit = Some(1)` → `None`, 1 row.
  - `a_missing_table_is_unavailable`: delete `parquet/task` → `Unavailable`.
  - `bad_sql_is_a_sql_error`: a SQLQuery selecting a column that does not exist → `Sql`.
  - `setup_secrets_are_redacted_from_errors`: setup `CREATE SECRET s (TYPE s3, KEY_ID 'AKIAXYZ', SECRET 'shh');` and bad SQL → the message contains neither `AKIAXYZ` nor `shh`.
  - `a_publish_during_the_query_runs_it_again`: write `.staging/.publishing` before calling → `Unavailable`; with a generation file bumped by a test hook between the two reads (expose a `#[cfg(test)]` hook on `Runner` that runs between steps 5 and 7 once) → the query is re-run and succeeds.
  - `busy_when_every_slot_is_taken`: `concurrency = 1`, hold the slot from the test → `Busy`.
- [ ] **Step 2:** `DUCKDB_AGENT_MODE=0 cargo test --bin fhir-pipes serve::query::run` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Same → PASS. `cargo test --bin fhir-pipes library::` still passes after the bind refactor.
- [ ] **Step 5:** Commit `serve: run a SQLQuery for $sql-run`.

---

### Task 4: Approved queries and the `/fhir` routes

**Files:**
- Create: `src/serve/query/catalog.rs`, `src/serve/query/http.rs`

**Interfaces:**
- Consumes: Tasks 2 and 3.
- Produces, in `catalog.rs`:
  - `pub enum Lookup<'a> { Run(&'a Library), NotApproved, NotFound }`
  - `pub struct Queries { pub catalog: Catalog, approve: Approve }` with `pub fn load(s: &warehouse::config::Settings, approve: &Approve) -> anyhow::Result<Queries>` (builds the catalog as `libraries_step::catalog` does; with `Listed`, each entry must resolve to a SQLQuery or it is an error naming the entry), `pub fn lookup(&self, canonical: &str) -> Lookup` (`url|version` split; `Catalog::get` error → `NotFound`; a SQLView or ViewDefinition → `NotApproved`; `Active` needs `status == "active"` on the Library — add `pub status: Option<String>` to `Library` in `library/mod.rs` if it is not there), and `pub fn approved(&self) -> Vec<&Library>`.
  - `Library` needs its source JSON for the Bundle: add `pub json: serde_json::Value` (set in `from_json`) if not already kept.
- Produces, in `http.rs`:
  - `pub struct QueryApi { pub queries: std::sync::RwLock<Arc<Queries>>, pub runner: Runner, pub settings_source: …, pub metrics: Arc<Metrics>, pub stdout: output::StdoutQueue }` (exact fields the builder's call; it must allow `reload()` in Task 5).
  - `pub fn routes(api: Arc<QueryApi>) -> axum::Router` with:
    - `GET|POST /fhir/$sql-run`
    - `GET /fhir/metadata` → CapabilityStatement (`status: "active"`, `kind: "instance"`, `fhirVersion: "4.0.1"`, `format: ["json"]`, `rest[0].mode: "server"`, `rest[0].operation: [{name: "sql-run", definition: "<base>/OperationDefinition/fhir-pipes-sql-run"}]`)
    - `GET /fhir/OperationDefinition/fhir-pipes-sql-run` → OperationDefinition with `base: "http://hl7.org/fhir/uv/sql-on-fhir/OperationDefinition/SQLRun"`, `code: "sql-run"`, `system: true`, `type: false`, `instance: false`, parameters `subjectCanonical` (in, 1..1, canonical), `_format` (in, 0..1, code), `_limit` (in, 0..1, integer), `return` (out, 1..1, Binary). `<base>` for the canonical is `https://fhir-pipes.local/fhir` unless the builder finds a better existing constant.
    - `GET /fhir/Library` → `Bundle` `type: "searchset"`, `total`, one `entry` per approved query with `resource` = the Library JSON.
  - Parameter handling: GET reads the query string; POST reads a `Parameters` body (`valueCanonical`, `valueCode`, `valueInteger`; max body 64 KB). Repeated or unknown names → 400 naming them. `_limit` < 1 or not an integer → 400.
  - Format: `_format` wins; else `Accept` of `application/json` → Json, `text/csv` → Csv, `application/x-ndjson` or anything else → Ndjson. Unknown `_format` → 400.
  - `Runner::run` is blocking: call it in `tokio::task::spawn_blocking`.
  - Each query pushes one stdout line: `query <canonical> <format> <status> <rows> rows <ms> ms[ truncated at <n>]`, and records `metrics.query_ended(status: u16, seconds: f64)`.
- Modify `src/serve/metrics.rs`: `pub fn query_ended(&self, status: u16, seconds: f64)`; render `fhir_pipes_queries_total{status="<code>"}` and a histogram `fhir_pipes_query_seconds` with buckets `0.1, 0.5, 1, 5, 10, 30, 60, +Inf`.

- [ ] **Step 1: Failing tests** (router tests with `tower::ServiceExt::oneshot`, or axum's test pattern already used in `serve/api.rs` tests; queries over the Task 3 helper warehouse):
  - `lookup_follows_the_rule`: draft `Q` under `Active` → `NotApproved`; `Listed([Q])` → `Run`; `All` → `Run`; the SQLView's url → `NotApproved`; unknown url → `NotFound`.
  - `a_listed_entry_that_is_not_a_sqlquery_is_an_error`: `Listed([<the SQLView url>])` → `Queries::load` errors naming it.
  - `a_versioned_canonical_resolves_and_an_unknown_version_is_404`.
  - `get_and_post_run_the_same_query`: both 200 with the same body and `content-type: application/x-ndjson`.
  - `format_precedence`: `_format=csv` with `Accept: application/json` → `text/csv`.
  - `unsupported_parameters_are_400`: each of `subjectResource`, `subjectReference`, `context`, `resource`, `parameters`, `patient`, `_since`, `header`, `foo` → 400, body `resourceType == "OperationOutcome"`.
  - `statuses`: no subject → 400; unknown → 404; draft under `Active` → 403; bad SQL → 422; `Busy` → 503 with `Retry-After`.
  - `truncation_header`: `max_rows = 1` → header `X-Fhir-Pipes-Truncated: max_rows=1`.
  - `metadata_and_library_bundle`: `/fhir/metadata` lists `sql-run`; `/fhir/Library` under `Active` with `Q` active lists exactly `Q`.
  - In `metrics.rs`: `queries_are_counted_by_status`.
- [ ] **Step 2:** `DUCKDB_AGENT_MODE=0 cargo test --bin fhir-pipes serve::query` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Same → PASS.
- [ ] **Step 5:** Commit `serve: /fhir routes for approved SQLQueries`.

---

### Task 5: Wiring into `serve`: tokens, startup, reload

**Files:**
- Modify: `src/serve/mod.rs`, `src/serve/api.rs` (reload hook after a run), `src/warehouse/mod.rs` (`ServeInputs` gains what is needed to build `Settings`: the loaded config `File` and its `base`, or a function that builds `Settings` with `Overrides { data: Some(data), ..Default::default() }`)
- Test: `tests/serve_query.rs` (new; reuse the `Serve` harness from `tests/serve.rs` by moving it into `tests/common/` if it is not shared already)

**Interfaces:**
- Consumes: Tasks 2–4.
- `App` gets `query_token: Option<String>` and `query_enabled: bool`. `auth` decides per path:
  - `/health`: open.
  - `/fhir/*`: with `[serve.query]` absent → 404 *after* the token check (so the presence of the API is not revealed to an unauthenticated caller when a token is set); accepted tokens: query token, serve token. If neither token is set, open (loopback only).
  - everything else: only the serve token, as today.
- Startup (`run`): with `[serve.query]`, build `Settings`, `Queries::load` (an error stops serve with a usage error), `Runner::new`. A non-loopback `--listen` with `[serve.query]` and no `FHIR_PIPES_QUERY_TOKEN` → usage error naming `FHIR_PIPES_QUERY_TOKEN`. The existing rule for `FHIR_PIPES_SERVE_TOKEN` stays.
- Reload: when a run ends (the thread in `Server::start`, after `run_ended`), call `QueryApi::reload()`: rebuild `Queries` from the same settings; on error keep the old one and push `query catalog not reloaded: <error>` to stdout.
- Add `FHIR_PIPES_QUERY_TOKEN` to the env vars the harness clears.

- [ ] **Step 1: Failing integration tests** in `tests/serve_query.rs` (skip without duckdb). Build the warehouse by running `fhir-pipes refresh --only views,libraries` as `tests/refresh_libraries.rs` does, with a config holding `[serve.query] approve = "all"`.
  - `a_query_returns_the_refreshs_rows`: `GET /fhir/$sql-run?subjectCanonical=<Q>` → 200, and the rows equal the refresh's `parquet/<q>` table read with duckdb in the Library's order.
  - `the_query_token_only_opens_fhir`: with both tokens set, the query token gets 200 on `/fhir/metadata` and 401 on `/status`, `/runs`, `POST /run`, `/metrics`; the serve token gets 200 on both kinds; no token gets 401 on `/fhir/metadata`.
  - `without_a_query_section_fhir_is_404`.
  - `a_non_loopback_listen_needs_the_query_token`: `--listen 0.0.0.0:0` with the serve token only → exit with an error naming `FHIR_PIPES_QUERY_TOKEN`.
  - `a_new_query_is_served_after_a_run`: add a second SQLQuery Library file, `POST /run` with `only: ["libraries"]`, wait for it to end, then the new canonical → 200 (before the run → 404).
- [ ] **Step 2:** `DUCKDB_AGENT_MODE=0 cargo test --test serve_query` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Same → PASS; `cargo test --test serve` still passes.
- [ ] **Step 5:** Commit `serve: enable the query API with [serve.query]`.

---

### Task 6: Docs and the whole suite

**Files:**
- Modify: `docs/serve.md` (new section "## Queries ($sql-run)": what it is, the config with every key and default, tokens, the routes, parameters, statuses, the truncation header, the remote-source limit during a push, the parameters TODO), `README.md` (one line under serve), `docs/libraries.md` (a pointer: approved SQLQueries can be served; `status = "active"`).

- [ ] **Step 1:** Write the docs. Every key, default and status in them must match the Global Constraints above.
- [ ] **Step 2:** `cargo fmt --check && cargo clippy --all-targets -- -D warnings` → clean.
- [ ] **Step 3:** `DUCKDB_AGENT_MODE=0 cargo test --no-fail-fast` → all pass.
- [ ] **Step 4:** Commit `docs: serve queries`.
