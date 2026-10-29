# SQL on FHIR Libraries

A Library is a FHIR resource that holds SQL. fhir-pipes reads two kinds,
defined by the SQL on FHIR implementation guide:

- a **SQLView** is a query over the warehouse's tables that other Libraries
  can read. It writes no table.
- a **SQLQuery** is a query whose result fhir-pipes writes as a table in the
  warehouse.

Each Library lists the things it reads in `relatedArtifact` entries of type
`depends-on`. An input is a ViewDefinition (read as the table its view
writes), another SQLView, another SQLQuery, or the built-in
`LocationHierarchy`. Each input has a label, the name the SQL uses for it.

The `refresh` command runs the SQLQuery Libraries in its `libraries` step
(see [refresh.md](refresh.md#libraries)). `libraries build` writes Library
JSON from SQL files, and `libraries sql` prints the SQL that runs one Library.

`fhir-pipes serve` can also run approved SQLQuery Libraries for a caller and
return their rows (see [serve.md](serve.md#queries-sql-run)). With the default
`approve = "active"`, a SQLQuery runs only when its `status` is `active`.

## Writing a Library

A Library source is a `.sql` file with a `.toml` file of the same name beside
it. `UgandaTasks.sql`:

```sql
SELECT h.admin1_name, h.admin2_name, count(*) AS tasks
FROM task AS t
JOIN hierarchy AS h ON h.id = t.location_id
GROUP BY h.admin1_name, h.admin2_name
```

`UgandaTasks.toml`:

```toml
url = "https://example.org/fhir/Library/UgandaTasks"
name = "UgandaTasks"
version = "0.1.0"
status = "draft"
kind = "SQLQuery"
title = "Tasks by district"

[[depends_on]]
label = "task"
resource = "https://example.org/fhir/ViewDefinition/IcrTask"

[[depends_on]]
label = "hierarchy"
resource = "https://healthcampaigns.org/fhir/analytics/Library/LocationHierarchy"
```

| Key | Required | Meaning |
|---|---|---|
| `url` | yes | The Library's canonical URL |
| `name` | yes | The Library's name. A SQLQuery's table is named from it as a view's is: `IcrCoverageByArea` becomes `coverage_by_area` with `strip_prefix = ["icr_"]` |
| `status` | yes | `draft` or `active` |
| `kind` | yes | `SQLView` or `SQLQuery` |
| `version`, `title`, `description` | no | Copied into the Library |
| `[[depends_on]]` | no | One entry per input: `label` (required) and `resource`, a canonical URL, with `|version` to pick a version |
| `[[parameter]]` | no | `name`, `type` and `documentation`. Only a SQLQuery may declare parameters. fhir-pipes keeps them in the Library and does not use them |

Any other key in the `.toml` file is an error. A `<name>.duckdb.sql` or
`<name>.postgresql.sql` file beside the `.sql` file adds SQL for that dialect.
The `.sql` file is the portable `application/sql` content.

### Labels

A label must be a lowercase SQL name matching `[a-z_][a-z0-9_]*`. The SQL
uses it unquoted, and PostgreSQL folds unquoted names to lowercase, so a
label with capitals would not match. `libraries build` and the loader both
refuse one.

A label is local to its Library. fhir-pipes puts each input in a CTE named by
the label, so two Libraries can use the same label for different inputs, and
a label does not replace a warehouse table of the same name.

### Portable SQL

fhir-pipes runs a Library's SQL in DuckDB for SQLQuery results and in
PostgreSQL for SQLViews (see [Database](refresh.md#database)). Write the
`application/sql` content so that both accept it, or add a dialect file for
each that needs its own. For a dialect, fhir-pipes uses the content typed
`application/sql;dialect=duckdb` or `application/sql;dialect=postgresql`,
and the portable content when there is none.

- Use standard SQL: `CAST(x AS VARCHAR)`, `COALESCE`, `CASE`, `WITH RECURSIVE`.
- Cast the columns of a recursive query's anchor to the types you want, so the
  recursive part's types match them. The built-in `LocationHierarchy` does
  this.
- In DuckDB 2.0, `sum` of a `BIGINT` gives a `HUGEINT`, which Parquet stores as
  `DOUBLE`. Cast the sum to a `DECIMAL` or `BIGINT` when you need an exact
  value.
- A trailing `;` is removed, so a query can end with one. A Library holds one
  statement: SQL with a second statement is refused (see [Sources](#sources)).

## libraries build

```
fhir-pipes libraries build <src> --out <dir>
```

Reads every `*.sql` file in `<src>` that has a `.toml` file beside it, and
writes `Library-<name>.json` in `<dir>`. The Library has the SQLView or
SQLQuery profile in `meta.profile`, the SQL as base64 `content` (with the
text also in the `sql-text` extension), and a `relatedArtifact` for each
input. It prints the number of files written. A `.sql` file with no `.toml`
beside it is not built, and a warning names it; a dialect file
(`<name>.duckdb.sql`, `<name>.postgresql.sql`) beside `<name>.sql` is not
warned about.

## libraries sql

```
fhir-pipes libraries sql <library> [--sources <source>...] [--warehouse <dir>] [--strip-prefix <prefix>...]
```

Prints the DuckDB SQL that runs one Library over a warehouse, to paste into
`duckdb`. `<library>` is a Library JSON file or a canonical URL
(`url` or `url|version`), which is resolved from the built-ins and
`--sources`.

| Option | Default | Meaning |
|---|---|---|
| `--sources` | none | Where canonicals resolve from after the built-ins; see [Sources](#sources) |
| `--warehouse` | `.` | The warehouse directory. Each input is read from `parquet/<table>/` in it |
| `--strip-prefix` | none | A prefix taken off table names, as the config's `tables.strip_prefix` does. Repeat for more |

The printed SQL reads each table with
`read_parquet('<warehouse>/parquet/<table>/**/*.parquet', hive_partitioning = true, union_by_name = true, hive_types_autocast = false)`.
With `hive_types_autocast = false`, a partition value such as `country=123`
is read as the string `123`, as it is in the warehouse's tables. A SQLView
the Library reads is written into the query; `location_hierarchy` and the
tables of ViewDefinitions and SQLQueries are read from their files. The
built-in `LocationHierarchy` is always read from `parquet/location_hierarchy/`,
whatever `--strip-prefix` says, as `refresh` reads it. Asked for
`LocationHierarchy` itself, the command prints `SELECT * FROM` that table,
after a comment saying that fhir-pipes reads the table the `locations` step
writes; the portable SQL is in `views/location/LocationHierarchy.sql`.

## Sources

A source is one of:

- a directory of JSON files, read with its subdirectories. A relative path
  resolves against the config file (or, for `libraries sql`, the current
  directory);
- a FHIR package `.tgz`, as a path or an https URL. An https package is
  downloaded again on every run that loads the Libraries;
- `name#version`, a package in the local FHIR package cache
  (`~/.fhir/packages`). fhir-pipes does not download it: it must be in the
  cache already.

A `refresh` loads the sources once, before the first step that needs them
(the `libraries` step, else the `database` step), and both steps use what it
loaded. The design asked for a package cache in the warehouse, so that a run
would not fetch an https package each time; that cache is not built yet.

A source's SQL is code that fhir-pipes runs. The libraries step runs it in
duckdb with the same file access as the refresh: a Library's query can read
any file the refresh can read, and DuckDB functions such as `read_csv` or
`read_text` read files by path. The database step also makes each SQLView
into a PostgreSQL view as the configured database user. Use only sources you
trust. An `https://`
package is fetched again on every run, so whoever controls that URL, or the
server behind it, decides the SQL the next refresh runs. Pin such a package
to a URL you control, or download it and use the local `.tgz`.

fhir-pipes limits what a Library can do beyond its query:

- a Library's SQL must be one statement. SQL with a `;` before more SQL
  (outside strings, quoted names and comments) is refused. Strings are
  `'…'`, `E'…'` with backslash escapes, and dollar quotes, `$$…$$` or
  `$tag$…$tag$`. It is refused when the Library is read and
  when `libraries build` builds it, naming the Library;
- the step's duckdb runs with `-init /dev/null`, so the user's
  `~/.duckdbrc` is not loaded.

These checks do not make an untrusted Library safe to run.

A source can hold ViewDefinitions, SQLView and SQLQuery Libraries, and
anything else, which is ignored. A file that is not JSON is skipped with a
warning. A ViewDefinition without a `url` is not used as a Library input,
since a Library cannot name it. A `refresh` can have such views in its views
directory, and they are skipped.

### Resolution order

A canonical resolves from:

1. the built-ins;
2. for `refresh`, the views directory;
3. each source in the order listed, and within a directory, in file-name
   order.

When the same URL and version is in more than one place, the first one wins.
A canonical with no `|version` must have one version only; with several, name
one as `url|version`.

Each artefact has a table of its own, named from its `name`, and for a SQLView
that name is also its PostgreSQL view's. Two versions of one canonical with
the same `name` would write the same table, so a Library pinned to one version
could read the other's result. The load refuses them, naming both as
`url|version`: give each version a name of its own.

The built-ins cannot be overridden. A source that defines the Location view or
`LocationHierarchy` stops the load, naming the URL.

A Library's dependencies are all resolved when the sources are loaded. A
dependency that is not found, a cycle, two Libraries that write the same
table, or a table name that is not a SQL name stops the step before any
SQL runs.

## Built-ins

| Canonical | What it is |
|---|---|
| `https://healthcampaigns.org/fhir/analytics/ViewDefinition/Location` | A ViewDefinition of `id`, `name`, `status`, `part_of`, `type_code`, `is_admin` and `admin_code`. It is the input of `LocationHierarchy` and writes no table. A Library other than `LocationHierarchy` cannot depend on it |
| `https://healthcampaigns.org/fhir/analytics/Library/LocationHierarchy` | A SQLView of each Location with its place in the `partOf` tree |

The Location view takes two constants, `admin_unit_type` (`admin-unit`) and
`admin_code_system` (`https://icr.healthcampaigns.org/identifiers/pcode`). They
decide which Locations are administrative units and which identifier is the
code. Their files are in `views/location/` in the source tree and are built
into the binary.

### LocationHierarchy

A Library that depends on `LocationHierarchy` reads the `location_hierarchy`
table that the `locations` step writes (see
[locations.md](locations.md#the-location_hierarchy-table)), in the `refresh`
step and in `libraries sql`. fhir-pipes does not run the SQLView's SQL. The
SQL is in the Library so that the columns and rules are written down, and it
is portable. The table has one row per Location, with the columns `id`,
`name`, `part_of`, `is_admin`, `depth`, `admin_level`, `admin0_name` to
`admin4_name`, `admin0_code` to `admin4_code`, `path` and `country`. `part_of`
is the id the Location's `partOf` reference gives, so an orphan Location keeps
the id of its missing parent. Where the table's rules differ from the SQL on
broken trees (duplicate ids, cycles, chains deeper than 12), the table is the
reference.

If a Library depends on `LocationHierarchy` and the `locations` step has not
written the table, the step stops and says to run the locations step.

## Where results go

A SQLQuery writes `parquet/<table>/`, with `<table>` named from the Library's
`name`. The Libraries run in dependency order, each in its own duckdb runs:
a `DESCRIBE` of the query, to see whether it has a `country` column, then a
`COPY` of its rows, plus one more run that writes the empty file of an empty
partitioned result. A SQLView is inlined into each query that reads it, so
its SQL is computed again for each of them. Each result is written to
staging with the other tables and published with them (see
[What a failed run leaves behind](refresh.md#what-a-failed-run-leaves-behind)).

- A result with a `country` column is partitioned by it:
  `country=<value>/part-0.parquet`, and `country=__HIVE_DEFAULT_PARTITION__`
  for rows with no country. The column is also in the files.
- Any other result is one file, `part-0.parquet`, in the table directory.
- An empty result still writes a file with its columns.
- The Library's `ORDER BY` is kept only in an unpartitioned result, which is
  one file. DuckDB's partitioned `COPY` does not keep the order within a
  partition, so a reader of a partitioned result should use its own
  `ORDER BY`.

A result cannot use the name of the table of a view in the views directory,
a `[[partition.lookup]]` table, `locations` or `location_hierarchy`. Such a
Library stops the step before anything runs, naming the Library and what
writes the table.

### In PostgreSQL

With a `[database]` section, the `database` step loads each SQLQuery result as
a table and each SQLView (not the built-in) as a view. The SQLViews are
dropped and recreated inside the load's transaction, so a reader of those views
waits until the whole load commits. The privileges granted on a SQLView's
view, its columns' privileges and its owner are kept (see [Database](refresh.md#database)
for the limits). The result of a SQLQuery that has been
removed from the sources stays in the database as a table; drop it by hand.
See [Database](refresh.md#database).
