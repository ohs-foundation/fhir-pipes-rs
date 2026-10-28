# Dashboard SQL as SQL on FHIR Libraries

Date: 2026-10-28. Status: approved.

## Why

The dashboards' SQL lives in Observable Framework data loaders (shell scripts that run DuckDB).
Each loader joins view tables to `locations.parquet`, reshapes them and writes a Parquet file for
the page. The logic is tied to Framework and to DuckDB, is repeated between sites, and cannot be
run by anyone who does not have the dashboards repo.

SQL on FHIR 3.0.0-ballot adds two Library profiles for this: **SQLView** (a named SQL query over
ViewDefinitions or other SQLViews, usable as a table) and **SQLQuery** (a SQL query that produces
a result). Both declare their inputs as `relatedArtifact` entries of type `depends-on`, with
`resource` set to the input's canonical URL and `label` set to the table name used in the SQL.
Library is an R4 resource, so these work with the FHIR 4.0.1 ICR IG.

The goal is to move the dashboard SQL into these Libraries, so the logic is defined once, in
FHIR artefacts, and any SQL engine can run it over the warehouse.

## Goals

- Dashboard SQL lives in SQLView and SQLQuery Libraries, not in Framework loaders.
- The Libraries are not DuckDB-specific. PostgreSQL is the most likely second engine; ClickHouse
  possibly.
- Every dashboard table can be joined to the administrative hierarchy without each query
  rebuilding it.
- A general Location strategy that works for any FHIR project, not only ICR.
- Uganda (`icr/dashboards/sites/uganda`) is the first site moved.

## Non-goals

- Running views or Libraries inside a FHIR server (`$sql-run`). fhir-pipes stays export-based:
  it exports, transforms and runs SQL over Parquet in the warehouse.
- Storing ViewDefinitions or Libraries on the HAPI R4 server. The files in git are the source.
- Porting the main ICR site (`dashboards/sites/main`) in this piece of work.
- Query parameters (`Library.parameter`). None of the Uganda queries need them.
- Changes to `locations.parquet`, the tiles, or the spatial loaders.

## Anti-goals

- Two versions of the hierarchy logic that run in production and can disagree.
- SQL that only runs in DuckDB without saying so.
- Walking the Location tree twice per refresh.

## Constraints

- The ICR IG is FHIR 4.0.1. ViewDefinition is not an R4 resource; the IG publishes the existing
  views as instances of the SoF logical model (`ig/input/fsh/viewdefinitions.fsh`), and Uganda's
  `country/uganda/fhir-pipes.toml` reads them from `ig/fsh-generated/resources`.
- `admin_level`, `admin0..4_name/code`, `country` and `path` are not on the Location resources.
  fhir-pipes derives them from the `partOf` chain in `resolve_hierarchy`
  (`src/index/hierarchy.rs`) during the locations step. A ViewDefinition reads one resource at a
  time and cannot follow `partOf`.
- Nigeria has about 349,000 Locations, so the hierarchy must be computed once per refresh.

## Decisions

### 1. Generic Location artefacts, shipped with fhir-pipes

Two artefacts that work for any FHIR project:

- **`Location`** (ViewDefinition over Location). One row per Location: `id`, `name`, `status`,
  `part_of` (`partOf.getReferenceKey(Location)`), `type_code`, `is_admin`, `admin_code`. No
  geometry. The project-specific parts are ViewDefinition `constant`s: the admin unit type code
  (ICR: `admin-unit`) and the identifier systems that hold the admin code (ICR: the `pcode`
  column's systems). fhir-pipes fills the constants from the profile (`admin_unit_type`,
  `admin_code_column`); another runner sets them itself.
- **`LocationHierarchy`** (SQLView). Depends on `Location`. A `WITH RECURSIVE` query that produces
  `id`, `name`, `part_of`, `is_admin`, `depth`, `admin_level`, `admin0..4_name`, `admin0..4_code`,
  `country` and `path`. `admin_level` counts only admin units on the chain, as the Rust does.
  `path` has the same format as `locations.parquet` today: the ids from the root down to and
  including the row itself, `/`-separated, with a leading `/` and no trailing one
  (`/uga/ug-r1/ug-d12`). "X and everything below it" is `path || '/' LIKE '%/X/%'`; "only below X"
  is `path LIKE '%/X/%'`. No list or struct types.

They live in the fhir-pipes repo (for example `views/location/`), are built into the binary, and
are always resolvable without configuration. A source that defines either canonical is an error:
the built-ins cannot be overridden, because fhir-pipes substitutes its Rust table for
`LocationHierarchy` and that is only correct for its own copy. A project that needs a different
hierarchy writes its own SQLView at its own canonical. (My call, from review.)

Canonical base: `https://healthcampaigns.org/fhir/analytics` (agreed for now). For example `https://healthcampaigns.org/fhir/analytics/Library/LocationHierarchy`.
The base does not name the repo, so the files can move to their own FHIR package later without
anything that depends on them changing.

### 2. fhir-pipes writes the hierarchy table from its existing walk

The locations step already walks the tree. It also writes a `location_hierarchy` table with the
columns and types `LocationHierarchy` declares. When fhir-pipes runs a Library that depends on
the `LocationHierarchy` canonical, it uses this table and does not run the SQLView's SQL. Other
engines run the SQL.

- The table includes Locations without geometry (unlike `locations.parquet` without
  `--keep-without-geometry`), because a Task or MeasureReport can point at one.
- The Rust output is the reference. On broken trees (cycles, duplicate ids, chains deeper than 12)
  fhir-pipes reports issues as now, and the SQL is not expected to match. Orphans (a `partOf` to
  a missing id) are expected to match: both treat them as roots.
- The SQL follows the Rust rules (`src/index/hierarchy.rs`), which the builder writes out in the
  SQLView's description: `admin_level` is null for a Location that is not an admin unit; the
  count of admin units is carried separately through non-admin Locations (so `admin_level` is not
  "parent's level + 1"); `admin0..4` are the first five admin units on the chain, root first;
  `country` is `admin0_code`, computed from the hierarchy for every row, including rows without
  geometry (not copied from `IndexRecord.country`, which is set only for rows with geometry); the
  recursion stops at depth 12.
- A test runs the SQLView's SQL over fixture Locations, in DuckDB and in PostgreSQL, and compares
  each with the Rust table, values and types. Fixtures: a clean tree; a non-admin Location in the
  middle of a chain; an orphan; more than 5 admin levels; an admin unit with no admin code. The
  test fails if columns or values differ.

fhir-pipes does not run the `Location` ViewDefinition during a refresh and writes no `location`
table: that would download and read the Locations a second time. Engines reading fhir-pipes'
warehouse read `location_hierarchy`. The `Location` view and the `LocationHierarchy` SQL are for
pipelines that do not use fhir-pipes, and for the test below. (My call.)

`locations.parquet` and the tiles stay as they are, for QGIS, the `registry` loader and
`admin.pmtiles`.

### 3. Views with the hierarchy joined in

For each view with a location column, a SQLView LEFT JOINs it to `LocationHierarchy`, adding
`location_name` (the joined Location's own `name`), `admin_level`, `admin0..4_name/code`,
`country` and `path`. Rows whose location is missing from
the hierarchy are kept with empty columns. Named with a suffix (`IcrCoverageByArea`); the bare
view is unchanged.

| View | Location column |
|---|---|
| IcrCoverage, IcrIndicatorStrata, IcrIndicatorValue, IcrLocationStatus | `location_id` |
| IcrCampaignCalendar | `location_id` |
| IcrTargetPopulation | `location_id` |
| IcrCampaignTask | `for_location_id` and `location_id`: joined twice, columns prefixed `for_` and `location_` (`for_admin1_name`, `location_path`); `country` from `for` |

Views with no location column (IcrCampaignActivity, IcrCoverageStrata, IcrIndicator,
IcrIndicatorGroup) get no SQLView; they are joined through other views.

A `…ByArea` SQLView selects the view's declared columns by name, then the hierarchy columns.
`country` comes from the hierarchy, not from the `country` column the warehouse adds to view
tables for partitioning, since another engine running the bare view has no such column. (My
call.)

### 4. Where artefacts live

| What | Where | Canonical base |
|---|---|---|
| `Location`, `LocationHierarchy` | fhir-pipes repo, built in | `https://healthcampaigns.org/fhir/analytics` |
| The 11 ICR views | unchanged, in the ICR IG | `https://icr.healthcampaigns.org` |
| `…ByArea` SQLViews and Uganda SQLQuery Libraries | `icr/dashboards/sites/uganda` (for example an `analytics/` folder) | their own, outside the IG (builder picks, e.g. `https://icr.healthcampaigns.org/uganda`) |

Artefacts outside the IG move into it once agreed as standard ICR views. Moving changes their
canonical, and the Libraries that depend on them are updated at that point.

Views and Libraries vary by project. A project's canonical views are the ones every
implementation of that project uses (for ICR, every country); they are defined in the project's
IG. Views and Libraries specific to one implementation or one analysis stay outside the IG. fhir-pipes therefore pulls them from IGs by canonical URL. A source in `fhir-pipes.toml`
can be:

- a published IG package (a FHIR NPM package, by `id#version` or a URL to its `package.tgz`),
- a local package or build output folder (as Uganda uses `ig/fsh-generated/resources` today),
- a plain folder of JSON files, for ad hoc Libraries not yet in an IG.

fhir-pipes resolves a canonical from, in order: the built-in artefacts, then the sources in the
order listed. It reads every ViewDefinition and Library in a source and indexes them by `url`
(and `version`, when a dependency names one). A Library's `depends-on` canonicals are resolved
the same way, so a project Library can depend on another IG's view. It never fetches from a FHIR
server. A downloaded package is cached in the warehouse so a refresh does not depend on the
network when the version is pinned.

### 5. Libraries run as a refresh step

A new `refresh` step, `libraries`, which runs once `views` and `locations` have finished
(`tiles` can still run alongside it). It reads the view tables and
`location_hierarchy`, runs the SQLViews and SQLQuery Libraries in dependency order in DuckDB, and
writes each SQLQuery result to the warehouse as Parquet. Failures go through the same
`last-run.json`, failure hooks, lock and `serve` schedule as other steps. Uganda's `warehouse.sh`
becomes `--only locations,views,libraries,tiles,hooks`.

### 6. Portable SQL

Library SQL uses `contentType: application/sql` and only syntax that DuckDB and PostgreSQL both
run. Not allowed in portable SQL: `QUALIFY`, `GROUP BY ALL`, list and struct types, list lambdas,
`regexp_extract`, DuckDB `read_parquet`. Where a query cannot be written portably, it carries one
`content` entry per dialect (`application/sql;dialect=duckdb`, `…;dialect=postgresql`), and the
runner picks its own.

Text that the current loaders cut out with regexes is better added as view columns, so the SQL
does not need regexes. See the Uganda list below.

### 7. Libraries are written as .sql files

The SQL is written and reviewed as plain `.sql` files. Each has a small metadata file next to it:
canonical `url`, `name`, `version`, `status`, whether it is a SQLView or a SQLQuery, and its
`depends-on` entries (label and canonical). A fhir-pipes command builds the Library JSON from
them. That JSON is the published form, and it meets the SQLQuery and SQLView profiles:
`content.data` holds the SQL in base64 and the plain-text extension holds the same SQL.

The runner reads the Library JSON, decodes the SQL, creates one view per `depends-on` `label` over
that input's table, and runs the SQL as written. The SQL refers only to labels, never to file
paths. A fhir-pipes command prints those view definitions and the SQL for any Library, so it can
be pasted into the `duckdb` shell for debugging.

The built-in `LocationHierarchy` is written the same way. `Location` is a ViewDefinition, kept as
JSON.

### 8. Catalog and PostgreSQL

`location_hierarchy` and every SQLQuery result are warehouse tables like the view tables: they go
in the manifest, get a DuckDB view in `catalog.sql`, and are loaded into PostgreSQL by the
`database` step when one is configured.

The `database` step also creates the SQLViews (the `…ByArea` views, and any others) as views in
PostgreSQL, using the Library's `application/sql` content, or its
`application/sql;dialect=postgresql` content when it has one. Each `depends-on` label is bound to
the loaded table of that input, as in DuckDB. A SQLView that fails to create fails the step, so
every refresh with a database shows whether the SQL runs on PostgreSQL (agreed).

`LocationHierarchy` is the exception: in PostgreSQL, as in DuckDB, the loaded
`location_hierarchy` table stands in for it, and its recursive SQL is not created there. This
keeps the hierarchy computed once. (My call.)

### 9. Framework reads the results

Framework pages read the Parquet files the `libraries` step wrote. A loader that remains is a
copy of one file. `registry.parquet.sh` (spatial functions) and `admin.pmtiles.sh` stay as they
are.

## First site: Uganda

Before the Uganda work starts, the 11 IG views are reviewed one by one. A view that every ICR
implementation would use stays in the IG. A view that only Uganda uses moves to the Uganda
folder, with a Uganda canonical. The first check found no Uganda-specific names, ids or codes in
`viewdefinitions.fsh`; the review is about whether other countries would use each view.

| Loader | Becomes | Depends on |
|---|---|---|
| `tasks.parquet.sh` | SQLQuery | `IcrCampaignTaskByArea` (`region` = `location_admin1_name`) |
| `targets.parquet.sh` | SQLQuery | `IcrCampaignActivity`, `IcrCampaignCalendarByArea`, `IcrTargetPopulation` |
| `population.parquet.sh` | SQLQuery | `LocationHierarchy` (`is_admin` in place of `type = 'admin-unit'`), `IcrTargetPopulation` |
| `registry.parquet.sh`, `admin.pmtiles.sh` | unchanged | `locations.parquet`, tiles |

The regexes in the current loaders are replaced by new columns on the shared IG views (agreed).
These only add columns; existing ones are unchanged, so the main site is not affected. The IG is
rebuilt (`sushi`) before the Uganda Libraries are written, and the Libraries are portable from
the first version.

- `activity_key` on IcrCampaignTask and IcrCampaignActivity: the last segment of the activity
  canonical, replacing `regexp_extract(activity, '([^/]+)$')` in tasks and targets. If the
  FHIRPath engine cannot take the last segment of a string, the builder comes back with
  alternatives.
- `code` and `code_text` on IcrTargetPopulation, from `Group.code`: `code` replaces parsing
  `target_code` from the Group id (`ichd-2026-10-<code>-<location>`), and `code_text` (which
  `targets.py` sets to the cleaned display) replaces the `regexp_replace` on `target_name`.
- `population` pivots by hard-coded Group id prefixes. `CASE WHEN` is portable; `GROUP BY ALL` is
  not and is written out.

The output columns and order of each Uganda result stay as they are now, so the pages do not
change. `population` switches from `locations.parquet` to `LocationHierarchy`, so admin units
without geometry now appear too. That is accepted. (My call.)

## Left to the builder

- File layout in the fhir-pipes repo and in `dashboards/sites/uganda`, and the `fhir-pipes.toml`
  key for Library folders.
- How fhir-pipes maps the profile onto the `Location` constants, including the profile's list of
  admin-code identifier systems.
- The warehouse layout of Library results. Default: the same layout as view tables
  (`parquet/<table>/country=…/`), partitioned by country when the result has a `country` column.
- Whether `LocationHierarchy` output has a `country` partition.
- How label bindings are kept local to each Library (see "Runner rules"), for example one DuckDB
  schema per Library run and, in PostgreSQL, rewriting labels to the qualified table names.

## Runner rules (from review)

These are my calls, from the Codex review of this design.

- **Staging.** The `libraries` step reads the staged tables of the current refresh (views and
  `location_hierarchy`), not the published ones, and writes its results to staging. It runs
  before `manifest`, `catalog` and `database`, so all of them see this refresh's results.
  `--only libraries` runs over the current published inputs and, like `--only views`, pulls in
  `database` when one is configured. `--only locations` also runs `libraries`, since
  `location_hierarchy` changed.
- **Missing versus empty inputs.** A `depends-on` whose table does not exist (its step was
  skipped, the view is not configured, or the canonical does not resolve) fails the Library and
  the step. A table that exists with zero rows is a valid input. When a view ran and produced no
  rows, the views step writes an empty table with the view's declared columns, so the two cases
  can be told apart. A direct `depends-on` on the built-in `Location` view is an error in
  fhir-pipes, since that table is never written; depend on `LocationHierarchy`.
- **Labels are local to a Library.** Two Libraries may use the same label for different
  canonicals. A label binding is visible only while its Library runs, and never replaces a
  warehouse table or another Library's binding. In PostgreSQL, persistent views must refer to
  real, schema-qualified tables, not temporary aliases.
- **Table identity.** Each resolved canonical (and version) maps to one warehouse table, named as
  view tables are today (from `name`, with `strip_prefix`). Two artefacts that map to the same
  table name are an error. A `depends-on` without a version that matches more than one version
  across the sources is an error.
- **PostgreSQL tables.** `location_hierarchy` and Library results have no resource key, so the
  `database` step replaces them in full each load rather than upserting. Changing one ancestor
  changes many descendants, so upserts would not fit anyway. Their column types are limited to
  types the PostgreSQL mapping supports; the builder extends the mapping (decimal, small
  integers) or casts in the Library SQL.
- **PostgreSQL views.** The existing loader drops a table when its columns change and fails if
  views depend on it. So the `database` step drops the SQLViews it created before loading tables
  and recreates them after, inside the same transaction as the table loads. A failed refresh
  leaves the database as it was.
- **Both engines in CI.** Creating views in PostgreSQL proves only that the SQLViews parse there.
  CI also runs every Library's SQL (the recursive `LocationHierarchy` SQL, the `…ByArea` views
  and the Uganda SQLQuery Libraries) over fixtures in DuckDB and in a throwaway PostgreSQL, and
  compares rows and types. That catches semantic differences a syntax rule misses, such as
  integer division and identifier case.
