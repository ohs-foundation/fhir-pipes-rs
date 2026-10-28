# SQL on FHIR Libraries Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run SQL on FHIR SQLView and SQLQuery Libraries as a `refresh` step over the Parquet warehouse, with a generic location hierarchy, and move the Uganda dashboard SQL into them.

**Architecture:** A new `src/library/` module reads Libraries (built from `.sql` sources), resolves `depends-on` canonicals across built-in artefacts, folders and IG packages, and composes each Library's SQL with its inputs as CTEs, so labels stay local to the Library. The locations step writes `location_hierarchy` from the existing `resolve_hierarchy` walk, and that table stands in for the built-in `LocationHierarchy` SQLView. The `libraries` refresh step runs SQLQuery Libraries with the `duckdb` command into staging; the `database` step loads their results and creates the SQLViews as PostgreSQL views.

**Tech Stack:** Rust (existing crate), `duckdb` CLI (as the tiles step uses it), PostgreSQL through `sqlx-postgres` (feature `postgres`), SUSHI for the ICR IG, Observable Framework loaders (bash).

**Spec:** `docs/superpowers/specs/2026-10-28-sql-on-fhir-libraries-design.md`

## Global Constraints

- Canonical base of the built-in artefacts: `https://healthcampaigns.org/fhir/analytics` (`…/ViewDefinition/Location`, `…/Library/LocationHierarchy`).
- Library profiles: `http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLView` and `…/SQLQuery`; plain-text extension `http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/sql-text`.
- `content.contentType` is `application/sql` or `application/sql;dialect=<duckdb|postgresql>`; `content.data` is base64 SQL and is required; the sql-text extension holds the same SQL.
- `relatedArtifact.type = depends-on`, `label` = table name in the SQL (required), `resource` = canonical, optionally `|version`.
- Portable SQL must not use `QUALIFY`, `GROUP BY ALL`, list or struct types, list lambdas, `regexp_extract`, `read_parquet`.
- `path` format: `/root/.../self`, leading `/`, no trailing `/` (as `Hierarchy::path`, `src/index/hierarchy.rs:83`).
- Hierarchy limits: `MAX_DEPTH = 12`, `ADMIN_COLUMNS = 5` (`src/index/hierarchy.rs:21-22`).
- `locations.parquet`, the tiles, `registry.parquet.sh` and `admin.pmtiles.sh` do not change.
- Uganda results keep their current columns and row order.
- Built-in canonicals cannot be defined by a source (error). Missing input table: error. Empty input table: valid.
- Write code like the surrounding code: comment density, `anyhow` errors with `context`, `usage()` for operator errors, `println!("== step")` progress lines.

## Review Focus

1. A Library's SQL ends with `;` or a `-- comment` line: composition must still produce valid SQL (Task 3 test `compose_handles_trailing_semicolon_and_comment`).
2. A view table exists with zero rows (a type absent from the server): the Library runs and writes an empty result with its columns (Task 6 test `an_empty_input_gives_an_empty_result`).
3. A Location whose `partOf` points at a dropped Location (cycle member): it is absent from `location_hierarchy`, and a `…ByArea` LEFT JOIN keeps its rows with empty admin columns (Task 4 test `rows_under_a_cycle_are_left_out`, Task 10 check).
4. DuckDB writes `sum(bigint)` as `DECIMAL(38,0)` and `count(*)` as `BIGINT`: the database step must load these types (Task 8 test `derived_tables_with_decimal_and_smallint_load`).
5. A Library's input view was renamed or left out of the views directory: the refresh fails naming the Library, label and canonical, before writing anything (Task 6 test `a_missing_input_fails_the_step_with_its_label`).

---

### Task 1: Library model and `.sql` sources

**Files:**
- Create: `src/library/mod.rs`, `src/library/source.rs`
- Modify: `src/main.rs` (add `mod library;`), `src/cli.rs` (add `Libraries(crate::library::LibrariesArgs)` next to `Views`)
- Test: `src/library/source.rs` (unit tests), `tests/libraries.rs`

**Interfaces:**
- Produces:
  - `pub enum Kind { SqlView, SqlQuery }`
  - `pub struct Dependency { pub label: String, pub canonical: String, pub version: Option<String> }`
  - `pub struct Library { pub url: String, pub version: Option<String>, pub name: String, pub kind: Kind, pub depends_on: Vec<Dependency>, pub content: Vec<(String /*contentType*/, String /*sql*/)>, pub json: serde_json::Value }`
  - `impl Library { pub fn from_json(v: &Value) -> Result<Library>; pub fn sql(&self, dialect: &str) -> Option<&str>; pub fn table(&self, strip_prefix: &[String]) -> String }`
    - `sql(dialect)`: the content with `application/sql;dialect=<dialect>`, else the one with plain `application/sql`.
    - `table`: `crate::views::run::table_name(&self.name, strip_prefix)`.
  - `pub fn build_source(sql_path: &Path) -> Result<Value>` in `source.rs`: reads `<stem>.sql` and `<stem>.toml`, returns Library JSON.
  - `pub fn build_dir(src: &Path, out: &Path) -> Result<usize>`: every `*.sql` with a sibling `.toml` → `out/Library-<name>.json`.
  - CLI: `fhir-pipes libraries build <src> --out <dir>` and `fhir-pipes libraries sql <library.json|canonical> [--sources …] [--warehouse <dir>]` (the second is Task 3).

Metadata file format (`<stem>.toml`); `<stem>.<dialect>.sql` siblings add dialect content:

```toml
url = "https://icr.healthcampaigns.org/uganda/Library/UgandaTasks"
name = "UgandaTasks"
version = "0.1.0"
status = "draft"          # draft | active
kind = "SQLQuery"         # SQLQuery | SQLView
title = "Uganda campaign tasks"   # optional
description = "…"                 # optional

[[depends_on]]
label = "task"
resource = "https://icr.healthcampaigns.org/uganda/Library/IcrCampaignTaskByArea"
```

- [ ] **Step 1: Write the failing tests** in `src/library/source.rs`:
  - `build_source_writes_a_sqlquery_library`: from a temp `t.sql` (`SELECT 1 AS x`) and `t.toml` as above, assert `resourceType == "Library"`, `meta.profile == ["http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLQuery"]`, `type.coding[0].code == "logic-library"`, `relatedArtifact[0] == {"type":"depends-on","label":"task","resource":"…IcrCampaignTaskByArea"}`, `content[0].contentType == "application/sql"`, base64-decoded `content[0].data == "SELECT 1 AS x"`, `content[0].extension[0] == {"url":"http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/sql-text","valueString":"SELECT 1 AS x"}`.
  - `a_dialect_file_adds_dialect_content`: `t.duckdb.sql` beside `t.sql` adds a second content with `application/sql;dialect=duckdb`.
  - `from_json_round_trips_and_picks_dialect`: `Library::from_json(build_source(..))`; `sql("duckdb")` returns the dialect SQL, `sql("postgresql")` returns the portable SQL.
  - `a_sqlview_with_parameters_is_refused`: metadata with `kind = "SQLView"` and a `[[parameter]]` table is an error mentioning "SQLView cannot declare parameters".
  - `a_dependency_without_label_is_refused`.
- [ ] **Step 2: Run** `cargo test --lib library::source` — FAIL (module missing).
- [ ] **Step 3: Implement** `Library`, `build_source`, `build_dir`, the `libraries build` subcommand. `from_json` accepts `meta.profile` containing the SQLView or SQLQuery profile URL, and errors otherwise. Trailing newline of the `.sql` file is trimmed before encoding.
- [ ] **Step 4: Add** `tests/libraries.rs::build_writes_one_library_per_sql_file` running the binary (`assert_cmd`-style, as `tests/views.rs` does) over a temp dir with two sources; asserts two `Library-*.json` files.
- [ ] **Step 5: Run** `cargo test --lib library && cargo test --test libraries` — PASS.
- [ ] **Step 6: Commit** `git commit -m "libraries: Library model and build from .sql sources"`

### Task 2: Canonical resolution

**Files:**
- Create: `src/library/resolve.rs`, `src/library/builtin.rs`
- Test: unit tests in `resolve.rs`

**Interfaces:**
- Consumes: `Library` (Task 1); `crate::views::run::{parse_view, View, table_name}`; `crate::fhir::package::Index::load(specs, flag, types)` for packages.
- Produces:
  - `pub enum Artifact { View(crate::views::run::View), Library(Library) }` with `fn url(&self) -> &str`, `fn version(&self) -> Option<&str>`, `fn table(&self, strip_prefix) -> String`.
  - `pub struct Catalog { /* url → Vec<Artifact> */ }`
  - `impl Catalog { pub fn load(sources: &[String], base: &Path, strip_prefix: &[String]) -> Result<Catalog>; pub fn get(&self, canonical: &str, version: Option<&str>) -> Result<&Artifact>; pub fn libraries(&self) -> Vec<&Library>; pub fn order(&self) -> Result<Vec<&Library>> }`
  - `pub const LOCATION_URL: &str = "https://healthcampaigns.org/fhir/analytics/ViewDefinition/Location";`
  - `pub const HIERARCHY_URL: &str = "https://healthcampaigns.org/fhir/analytics/Library/LocationHierarchy";`
  - `pub const HIERARCHY_TABLE: &str = "location_hierarchy";`
  - `builtin::artifacts() -> Vec<Artifact>` (filled in Task 3; returns an empty Vec here).

A source string is one of: a directory (every `*.json` that is a ViewDefinition, SUSHI's `…/ViewDefinition` logical form via `parse_view`, or a SQLView/SQLQuery Library); a path or `https://` URL to a `package.tgz`; `name#version` in the FHIR package cache. Packages and `name#version` go through `crate::fhir::package::Index::load(&[spec], "libraries", &["Library", "Binary"])`, then the same ViewDefinition/Library filter. Relative directory and `.tgz` paths resolve against `base` (the config file's directory).

- [ ] **Step 1: Write the failing tests** in `resolve.rs`:
  - `a_canonical_resolves_from_the_first_source_that_has_it`: two dirs with the same Library url and different versions; `get(url, Some("2"))` returns the second; `get(url, None)` is an error containing "more than one version".
  - `a_source_cannot_define_a_built_in`: a dir with a Library whose url is `HIERARCHY_URL` → `load` error containing "built in".
  - `two_artifacts_with_one_table_are_refused`: Library `IcrFoo` and view `IcrFoo` with `strip_prefix = ["icr_"]` → error containing "both write table foo".
  - `the_reserved_tables_are_refused`: a Library named `Locations` or `LocationHierarchy` (at another url) → error.
  - `order_puts_dependencies_first_and_finds_cycles`: A depends on B (SQLView), B on a view → order `[B, A]`; A↔B → error containing "cycle".
  - `an_unresolved_dependency_names_the_library_and_label`: error text contains library name, label and canonical.
  - `a_sushi_logical_model_view_is_read` using `tests/fixtures/ig` (existing fixture IG output).
- [ ] **Step 2: Run** `cargo test --lib library::resolve` — FAIL.
- [ ] **Step 3: Implement** `Catalog::load`, `get`, `order` (depth-first topological order over SQLView dependencies; views are leaves). Reserved table names: `locations`, `location_hierarchy`.
- [ ] **Step 4: Run** `cargo test --lib library::resolve` — PASS.
- [ ] **Step 5: Commit** `git commit -m "libraries: resolve canonicals from built-ins, folders and IG packages"`

### Task 3: SQL composition, built-in Location artefacts, `libraries sql`

**Files:**
- Create: `src/library/compose.rs`, `views/location/Location.json`, `views/location/LocationHierarchy.sql`, `views/location/LocationHierarchy.toml`
- Modify: `src/library/builtin.rs` (embed with `include_str!`; the SQLView JSON is built at runtime with `build_source`'s in-memory form `build_from(sql: &str, meta: &str) -> Result<Value>`, which Task 1's `build_source` calls)
- Test: unit tests in `compose.rs`

**Interfaces:**
- Consumes: `Catalog`, `Library`, `Artifact` (Task 2).
- Produces:
  - `pub enum Bind { Relation(String) }` — the SQL relation expression an input is read from.
  - `pub fn compose(lib: &Library, dialect: &str, catalog: &Catalog, bind: &dyn Fn(&Artifact) -> Result<Bind>) -> Result<String>`

Composition rule (shared by DuckDB and PostgreSQL, and what keeps labels local):

```
WITH "<label1>" AS (<input1>), "<label2>" AS (<input2>)
SELECT * FROM (
<library sql, trailing whitespace and ';' removed>
) AS q
```

where `<input>` is `SELECT * FROM <bind(artifact)>` for a view, for `LocationHierarchy` (always bound to its table, never its SQL), and for a SQLView that the caller binds to a relation; otherwise, for a SQLView, the recursively composed SQL of that SQLView. A newline always precedes the closing `)`, so a trailing `--` comment cannot swallow it. For a SQLQuery, the outer `SELECT * FROM (…) AS q` keeps the Library's `ORDER BY`; the DuckDB runner relies on DuckDB preserving subquery order for `COPY` (asserted in Task 6).

`LocationHierarchy` SQL: a portable `WITH RECURSIVE` over label `location` (columns from `Location.json`: `id, name, status, part_of, type_code, is_admin, admin_code`). Anchor: rows whose `part_of` is null or not an `id` in `location` (orphans are roots). Step: children of the previous level while `depth < 11` (so depth ≤ 11 = 12 levels). Carried columns: `depth`, `admin_count` (adds 1 only when `is_admin`), `admin0_id..admin4_id` (filled at position `admin_count` when `is_admin` and `admin_count < 5`), `path`. Output: `id, name, part_of, is_admin, depth, admin_level (CASE WHEN is_admin THEN admin_count - 1 END), admin0..4_name, admin0..4_code (joined back by admin*_id), country (= admin0_code), path`. Integer columns cast to `INTEGER`; `admin_level` to `SMALLINT` is not used (keep `INTEGER`). Duplicate ids: `SELECT DISTINCT ON`-free; the input is assumed duplicate-free (fhir-pipes' Rust is the reference for broken trees).

`Location.json` constants: `admin_unit_type` (default `admin-unit`), `admin_code_system` (default `https://icr.healthcampaigns.org/identifiers/pcode`). Columns: `id = getResourceKey()`, `name`, `status`, `part_of = partOf.getReferenceKey(Location)`, `type_code = type.coding.first().code`, `is_admin = type.coding.first().code = %admin_unit_type`, `admin_code = identifier.where(system = %admin_code_system).value.first()`. (The Rust reads the first of several systems; the view reads one. This differs only when Locations carry the second ICR system; noted in the view's description.)

- [ ] **Step 1: Write the failing tests** in `compose.rs`:
  - `labels_are_local_ctes`: SQLQuery Q depends on view V as label `t`; `compose` returns text starting `WITH "t" AS (SELECT * FROM <bound V>)`.
  - `a_sqlview_dependency_is_inlined_with_its_own_labels`: Q → SQLView S (label `s`) → view V (label `t`); output contains `"s" AS (WITH "t" AS (`.
  - `location_hierarchy_is_always_bound_to_its_table`: a dependency on `HIERARCHY_URL` becomes `SELECT * FROM <bind>`, and `bind` was called with the hierarchy artifact.
  - `compose_handles_trailing_semicolon_and_comment`: SQL `SELECT 1 AS x; -- end` composes to text that `duckdb` runs (skip without `duckdb`, as `tests/refresh_catalog.rs::have_duckdb` does).
  - `a_direct_dependency_on_the_location_view_is_refused`: error containing "depend on LocationHierarchy".
  - `the_builtins_parse`: `builtin::artifacts()` has the Location view and the `LocationHierarchy` SQLView at the constants' urls.
- [ ] **Step 2: Run** `cargo test --lib library::compose` — FAIL.
- [ ] **Step 3: Implement** `compose`, write the three `views/location/` files, fill `builtin::artifacts()`.
- [ ] **Step 4: Implement** `fhir-pipes libraries sql <library> [--sources S…] [--warehouse DIR]`: prints the composed DuckDB SQL with each input bound to `read_parquet('<warehouse>/parquet/<table>/**/*.parquet', hive_partitioning=true, union_by_name=true)`.
- [ ] **Step 5: Run** `cargo test --lib library` — PASS.
- [ ] **Step 6: Commit** `git commit -m "libraries: compose SQL with local labels; built-in Location and LocationHierarchy"`

### Task 4: `location_hierarchy` table from the walk

**Files:**
- Create: `src/write/hierarchy.rs`
- Modify: `src/write/mod.rs`, `src/transform.rs` (`transform_with`), `src/cli.rs` (`TransformArgs`: add `hierarchy_out: Option<PathBuf>`, flag `--hierarchy-out`), `src/warehouse/fhir.rs` and `src/warehouse/files.rs` (pass `stage.tables_dir().join("location_hierarchy")`)
- Test: unit tests in `src/write/hierarchy.rs`; `tests/transform.rs`

**Interfaces:**
- Consumes: `Index` (`records`, `hierarchy`), `Hierarchy::{get, path, admin_names, admin_codes, country}` (`src/index/hierarchy.rs`).
- Produces: `pub fn write_hierarchy(index: &Index, out_dir: &Path) -> crate::error::Result<usize>` — writes `out_dir/country=<admin0_code>/part-0.parquet` (rows with no country in the partition the views step uses for nulls, `partition::write`'s null segment), returns rows.

Columns and Arrow types (the `LocationHierarchy` SQLView declares the same): `id` Utf8, `name` Utf8, `part_of` Utf8, `is_admin` Boolean, `depth` Int32, `admin_level` Int32 (null when not admin), `admin0_name..admin4_name` Utf8, `admin0_code..admin4_code` Utf8, `country` Utf8, `path` Utf8. One row per record whose `hierarchy.get(i)` is `Some`, whether or not it has geometry; `part_of` is the resolved parent's id (null for roots and orphans).

- [ ] **Step 1: Write the failing tests** in `src/write/hierarchy.rs`, using `hierarchy.rs`'s `rec` fixture style:
  - `writes_every_resolved_location_including_ones_without_geometry`.
  - `admin_level_is_null_for_non_admin_and_counts_through_them`: chain `country(admin) → zone(non-admin) → district(admin)`; district `admin_level == 1`, zone null, `admin1_name == district name`.
  - `country_comes_from_the_admin0_code_for_every_row`.
  - `rows_under_a_cycle_are_left_out`.
  - `path_matches_locations_parquet`: equals `Hierarchy::path` for each row.
- [ ] **Step 2: Run** `cargo test --lib write::hierarchy` — FAIL.
- [ ] **Step 3: Implement** `write_hierarchy`; call it from `transform_with` after `write_dataset` when `args.hierarchy_out` is set. The warehouse locations step always sets it.
- [ ] **Step 4: Add** `tests/transform.rs::transform_writes_the_hierarchy_table` (`--hierarchy-out`) on `tests/fixtures/snapshot`.
- [ ] **Step 5: Run** `cargo test --lib write::hierarchy && cargo test --test transform` — PASS.
- [ ] **Step 6: Commit** `git commit -m "transform: write location_hierarchy from the partOf walk"`

### Task 5: Hierarchy parity test, DuckDB and PostgreSQL

**Files:**
- Create: `tests/location_hierarchy.rs`, `tests/fixtures/hierarchy/locations.ndjson`
- Modify: `.github/workflows/*.yml` (the test job: install the `duckdb` CLI, pinned version, before `cargo test`)

**Interfaces:**
- Consumes: `fhir-pipes transform --hierarchy-out` (Task 4); `fhir-pipes views run views/location/Location.json` with `--constant` overrides is **not** needed: the fixture uses the default constants.

Fixture Locations: a clean 3-level admin tree; a non-admin Location between two admin units; an orphan (`partOf` to a missing id); a chain with 6 admin levels; an admin unit with no pcode.

- [ ] **Step 1: Write the tests**:
  - `the_sql_matches_the_rust_in_duckdb`: run the `Location` view over the fixture to Parquet; run `LocationHierarchy`'s composed SQL in `duckdb` with label `location` bound to it; compare every row and column with `transform --hierarchy-out`'s table (`SELECT … EXCEPT SELECT …` both ways returns 0 rows; column names and DuckDB types equal). Skip without `duckdb`.
  - `the_sql_matches_the_rust_in_postgresql`: load both into `$FHIR_PIPES_TEST_DATABASE_URL` (a schema per test, as `tests/refresh_database.rs` does), compare the same way. Skip without the variable.
- [ ] **Step 2: Run** `cargo test --test location_hierarchy` — iterate on `LocationHierarchy.sql` until PASS in both.
- [ ] **Step 3: Commit** `git commit -m "Test LocationHierarchy's SQL against the Rust walk in DuckDB and PostgreSQL; duckdb in CI"`

### Task 6: The `libraries` refresh step

**Files:**
- Create: `src/warehouse/libraries_step.rs`
- Modify: `src/warehouse/mod.rs` (`Step::Libraries` between `Tiles` and `Manifest`, `name()`, `steps()`, `refresh()`, `required_tools`, module docs), `src/warehouse/config.rs` (`libraries: Vec<String>` in the file config and `Settings`, resolved against the config dir), `docs/refresh.md`
- Test: `tests/refresh_libraries.rs`, unit tests in `mod.rs` (`steps_of`)

**Interfaces:**
- Consumes: `Catalog::load`, `Catalog::order`, `compose` (Tasks 2–3); `stage.read(table)`, `stage.table(table)`; `tools::duckdb`; `partition::partition_key`.
- Produces: `pub(super) fn libraries_step(s: &Settings, stage: &stage::Stage) -> Result<BTreeSet<String>>` — the result tables written.

Behaviour:
- Runs only when `s.libraries` is non-empty (the built-ins alone produce no SQLQuery). Runs after the views/tiles join in `refresh()`, before `manifest`.
- Sources: `s.views` (the views directory) first, then `s.libraries` in order.
- For each SQLQuery in `order()`: every input view or the hierarchy binds to `read_parquet([<files>], hive_partitioning=true, union_by_name=true)` from `stage.read(table)`; an input whose directory does not exist fails with "`<Library>`: input `<label>` (`<canonical>`) has no table `<table>`; …". SQLView inputs are inlined (compose). One `duckdb` process per SQLQuery: `COPY (<composed>) TO '<stage.table(t)>' (FORMAT PARQUET, PARTITION_BY (country))` when the result has a `country` column (checked with `DESCRIBE`), else to `<stage.table(t)>/part-0.parquet`.
- `steps()`: `--only libraries` adds `Manifest`, `Catalog`, `Database`; `--only views` and `--only locations` add `Libraries`; the default set includes `Libraries`.
- `required_tools`: `duckdb` for `Step::Libraries` when `s.libraries` is non-empty.

- [ ] **Step 1: Write the failing tests** in `tests/refresh_libraries.rs` (file source `--input`, as `tests/refresh_files.rs`), skip without `duckdb`:
  - `a_sqlquery_result_is_published_with_the_views`: one view, one SQLQuery over it; after `refresh`, `parquet/<result>/` exists with expected rows, and is listed in `manifest.json`.
  - `results_read_this_runs_tables`: second refresh with changed input data; the result reflects the new data.
  - `an_empty_input_gives_an_empty_result`.
  - `a_missing_input_fails_the_step_with_its_label`: `last-run.json` has `failed_step == "libraries"`; no table published.
  - `order_by_is_kept`: result rows in the Library's `ORDER BY` order.
  - `a_byarea_view_joins_the_hierarchy`: SQLView joining a view to `LocationHierarchy`, SQLQuery over it; admin columns filled from the fixture.
- [ ] **Step 2: Add** unit tests to `mod.rs`: `libraries_follow_views_and_locations` (`steps_of(&["--only","views"])` and `["--only","locations"]` contain `Libraries`; `["--only","libraries"]` contains `Database` and `Manifest`).
- [ ] **Step 3: Run** `cargo test --test refresh_libraries && cargo test --lib warehouse` — FAIL.
- [ ] **Step 4: Implement** config, step, wiring.
- [ ] **Step 5: Run** the same — PASS.
- [ ] **Step 6: Commit** `git commit -m "refresh: libraries step runs SQLQuery Libraries into the warehouse"`

### Task 7: Catalog and manifest coverage

**Files:**
- Modify: `src/warehouse/catalog.rs` only if needed
- Test: `tests/refresh_libraries.rs`

- [ ] **Step 1: Write the test** `results_and_the_hierarchy_are_in_the_catalog`: with `[catalog]`, `catalog.sql` has a view for `location_hierarchy` and for the result table; `manifest.json` lists both with row counts.
- [ ] **Step 2: Run** — expected PASS already (`stage.tables()` lists every table dir). If it fails, add the tables in `catalog::tables`.
- [ ] **Step 3: Commit** `git commit -m "Test that results and location_hierarchy reach the catalog and manifest"`

### Task 8: PostgreSQL: derived tables and SQLViews

**Files:**
- Modify: `src/warehouse/database.rs`, `src/warehouse/mod.rs` (database table list)
- Test: `tests/refresh_database.rs`

**Interfaces:**
- Consumes: `Catalog`, `compose` with dialect `postgresql`.
- Produces: `database::Table` entries for `location_hierarchy` and each result table with `key: None`, `time_dependent: true` (loaded in full in both modes); `pub fn sql_views(s: &Settings, catalog: &Catalog, schema: &str) -> Result<Vec<(String /*name*/, String /*CREATE VIEW sql*/)>>`.

Rules:
- `pg_type` adds `Decimal128(p, s)` → `numeric(p,s)`, `Int16`/`Int8` → `smallint`, `Float32` → `real`; the COPY row encoder handles them.
- Each SQLView (except `LocationHierarchy`) becomes `CREATE VIEW <schema>.<table> AS <compose(…, "postgresql", bind)>` where `bind` gives `<schema>.<table>` of the loaded input, and a SQLView input binds to its own view (not inlined). Each view gets `COMMENT ON VIEW … IS 'fhir-pipes SQLView <canonical>'`.
- In the load transaction: first drop every view in the schema whose comment starts `fhir-pipes SQLView ` (`DROP VIEW … CASCADE` is not used; drop in reverse dependency order), then load tables, then create the views in `order()`. A view that fails to create rolls back the transaction and fails the step with the Library name and PostgreSQL's message.

- [ ] **Step 1: Write the failing tests** in `tests/refresh_database.rs` (skip without `$FHIR_PIPES_TEST_DATABASE_URL`):
  - `derived_tables_with_decimal_and_smallint_load`: a SQLQuery with `sum(x)` (bigint) and `CAST(1 AS SMALLINT)`; loaded types are `numeric` and `smallint`.
  - `sqlviews_are_created_as_views`: a `…ByArea`-style SQLView exists in the schema and returns joined rows.
  - `a_column_change_recreates_the_views`: second refresh adds a column to the input view; refresh succeeds, view still queries.
  - `a_view_that_fails_rolls_back_the_load`: a SQLView with invalid PostgreSQL SQL; step fails, table rows unchanged from the previous run.
- [ ] **Step 2: Run** `cargo test --test refresh_database` — FAIL.
- [ ] **Step 3: Implement**.
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `git commit -m "database: load derived tables and create SQLViews as PostgreSQL views"`

### Task 9: Documentation

**Files:**
- Create: `docs/libraries.md`
- Modify: `docs/refresh.md` (step list, `libraries` config key, staging), `docs/locations.md` (the `location_hierarchy` table), `README.md` (command list)

- [ ] **Step 1: Write** `docs/libraries.md`: the `.sql` + `.toml` source format, `libraries build`, `libraries sql`, source kinds and resolution order, label locality, the built-ins and their constants, portable SQL rules, how fhir-pipes uses `location_hierarchy` for `LocationHierarchy`.
- [ ] **Step 2: Commit** `git commit -m "Document Libraries and the location_hierarchy table"`

### Task 10: ICR IG columns (icr repo)

**Files (in `/Volumes/maximus/github/icr`, on a new branch):**
- Modify: `ig/input/fsh/viewdefinitions.fsh` (IcrCampaignTask, IcrCampaignActivity, IcrTargetPopulation)

Columns, appended after the existing ones (no existing column changes):
- IcrCampaignTask: `activity_key` = `instantiatesCanonical.replaceMatches('^.*/', '')`, type `string`.
- IcrCampaignActivity (inside `select[1]`): `activity_key` = `instantiatesCanonical.first().replaceMatches('^.*/', '')`, type `string`.
- IcrTargetPopulation: `code` = `code.coding.first().code`, type `code`; `code_text` = `code.text`, type `string`.

- [ ] **Step 1: Edit** the FSH, run `sushi` in `ig/` — builds without errors.
- [ ] **Step 2: Check** with `fhir-pipes views run ig/fsh-generated/resources --input data/raw/hapi-gcp --output ndjson --out <scratch>`: `activity_key` equals the old `regexp_extract(activity, '([^/]+)$')` for every row; `code` equals the id-parsed `target_code` for every `ichd-2026-10-*` Group; `code_text` equals the trimmed `target_name`. (Compare in `duckdb`.)
- [ ] **Step 3: Commit** in icr: `git commit -m "IG views: activity_key, Group code and code_text columns"`

### Task 11: Uganda Libraries (icr repo)

**Files (icr):**
- Create: `dashboards/sites/uganda/analytics/*.sql` and `*.toml`:
  - SQLViews: `IcrCoverageByArea`, `IcrIndicatorStrataByArea`, `IcrIndicatorValueByArea`, `IcrLocationStatusByArea`, `IcrCampaignCalendarByArea`, `IcrTargetPopulationByArea`, `IcrCampaignTaskByArea` (two joins, `for_`/`location_` prefixes, `country` from `for`).
  - SQLQueries: `UgandaTasks`, `UgandaTargets`, `UgandaPopulation`.
  - Canonical base `https://icr.healthcampaigns.org/uganda`, `status = "draft"`.
- Modify: `country/uganda/fhir-pipes.toml` (`libraries = ["../../dashboards/sites/uganda/analytics/build"]`), `country/uganda/warehouse.sh` (`libraries build` before refresh; `--only locations,views,libraries,tiles,hooks`), `.gitignore` (the `build/` folder)

Each `…ByArea` SQLView selects the view's declared columns by name, then `location_name, admin_level, admin0..4_name, admin0..4_code, country, path` from `LocationHierarchy` (LEFT JOIN on the view's location column). Each SQLQuery reproduces its loader's columns and `ORDER BY` exactly, using `activity_key`, `code`, `code_text`, `is_admin`, `coalesce(admin1_name, location_name)`, and no DuckDB-only syntax.

- [ ] **Step 1: Write** the sources; run `fhir-pipes libraries build`.
- [ ] **Step 2: Capture** the old outputs: run the three current loaders into `<scratch>/old/`.
- [ ] **Step 3: Run** `country/uganda/warehouse.sh` (needs the Uganda credentials; ask the user to run it with `! …` if they are not in the environment).
- [ ] **Step 4: Compare** each result table with its old output in `duckdb`: same columns in order; `EXCEPT` both ways 0 rows, except `UgandaPopulation`, which may add admin units without geometry (list them for the user).
- [ ] **Step 5: Commit** in icr: `git commit -m "Uganda: dashboard SQL as SQL on FHIR Libraries"`

### Task 12: Uganda loaders read the results (icr repo)

**Files (icr):**
- Modify: `dashboards/sites/uganda/src/data/tasks.parquet.sh`, `targets.parquet.sh`, `population.parquet.sh` — each becomes a copy of its result table (`duckdb -c "COPY (SELECT * FROM read_parquet('$W/<table>/**/*.parquet', hive_partitioning=true, union_by_name=true) ORDER BY …) TO '/dev/stdout' (FORMAT PARQUET)"`, or `cat` of a single file when unpartitioned), keeping the `DATA` default and header comment style.
- Unchanged: `registry.parquet.sh`, `admin.pmtiles.sh`.

- [ ] **Step 1: Edit** the loaders.
- [ ] **Step 2: Build** the site (`npm run build` in `dashboards/` for the uganda site, as its README says) and open the pages; the tables and charts show the same numbers as before.
- [ ] **Step 3: Commit** in icr: `git commit -m "Uganda loaders read the Library results"`

### Task 13: Review of the IG views (decision for the user)

**Files:** none changed.

- [ ] **Step 1: Write** a short table for the user: each of the 11 IG views, which sites and warehouses read it (`grep` in `dashboards/sites/*` and `country/*`), and whether anything in it is Uganda-specific. The user decides which, if any, move to `dashboards/sites/uganda/analytics`; moving one is a follow-up change.
