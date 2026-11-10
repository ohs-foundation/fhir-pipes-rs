# SQL on FHIR Libraries

A Library is a FHIR resource that holds SQL. fhir-pipes reads two kinds,
defined by the SQL on FHIR implementation guide:

- a **SQLView** is a query over the warehouse's tables that other Libraries
  can read. It writes no table.
- a **SQLQuery** is a query whose result fhir-pipes writes as a table in the
  warehouse.

A Library lists its inputs as `depends-on` entries: a ViewDefinition (read as
the table its view writes), a SQLView, a SQLQuery, or the built-in
`LocationHierarchy`. Each input has a label, the name the SQL uses for it.

`refresh` runs the SQLQuery Libraries in its `libraries` step (see
[refresh.md](refresh.md#libraries)). `fhir-pipes serve` can run approved
SQLQuery Libraries for a caller with `$sql-run` (see
[serve.md](serve.md#queries-sql-run)).

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
status = "draft"
kind = "SQLQuery"

[[depends_on]]
label = "task"
resource = "https://example.org/fhir/ViewDefinition/IcrTask"

[[depends_on]]
label = "hierarchy"
resource = "https://healthcampaigns.org/fhir/analytics/Library/LocationHierarchy"
```

`url`, `name`, `status` (`draft` or `active`) and `kind` (`SQLView` or
`SQLQuery`) are required. A SQLQuery's table is named from `name`. Each
`[[depends_on]]` has a `label` (matching `[a-z_][a-z0-9_]*`) and a `resource`,
a canonical URL (add `|version` to pick one). `version`, `title` and
`description` are also accepted; SQLQueries also accept `[[parameter]]` entries, each with a `name`, a `type` and an optional `documentation`; see [Parameters](#parameters). Any other key is an error.

SQLQueries run in DuckDB; SQLViews run inline there and can also become PostgreSQL views. So write standard
SQL that both accept, or add a `<name>.duckdb.sql` or `<name>.postgresql.sql`
file beside the `.sql` file. A Library holds one statement.

### Parameters

A SQLQuery declares the values a caller can give it under `[[parameter]]`,
and reads each as `:name` in its SQL, as the SQL on FHIR SQLQuery profile
writes them:

```toml
[[parameter]]
name = "year"
type = "integer"
documentation = "The year the campaign started"

[[parameter]]
name = "state"
type = "string"
```

```sql
SELECT * FROM coverage
WHERE (:year IS NULL OR year = :year)
  AND (:state IS NULL OR state = :state)
```

A parameter that is not given a value is NULL. Write each condition as
above, `(:name IS NULL OR column = :name)`, so the query returns every row
when no value is given and the matching rows when one is.

| Who runs the Library | What a parameter's value is |
|---|---|
| `serve`'s `$sql-run` | What the caller gives it ([serve.md](serve.md#a-librarys-own-parameters)); NULL when not given |
| `refresh` | NULL: the table holds what the query returns with no value given |
| `import run` | The import package's parameter of the same name, which must exist ([import.md](import.md#in-sqlquery-libraries)) |

| `type` | duckdb type |
|---|---|
| `string`, `code`, `id`, `uri`, `url`, `canonical`, `markdown`, `oid`, `uuid` | `VARCHAR` |
| `integer`, `positiveInt`, `unsignedInt`, `integer64` | `BIGINT` |
| `decimal` | `DOUBLE` |
| `boolean` | `BOOLEAN` |
| `date` | `DATE` |
| `dateTime`, `instant` | `TIMESTAMPTZ` |
| `time` | `TIME` |

A value is never written into the SQL. For `$sql-run` and `import run`, each
`:name` becomes `getvariable('name')`, and the script sets that variable
from a value passed to duckdb in its environment, cast to the parameter's
type (in `$sql-run`, to a NULL of that type when no value is given). A
refresh, which has no values, writes each `:name` as `CAST(NULL AS type)`.
A `:name` in a string, a quoted identifier or a comment is left as it is,
and `::` is a cast. A parameter takes one value; there are no lists. A SQLView cannot
declare parameters.

## libraries build

```
fhir-pipes libraries build <src> --out <dir>
```

Writes `Library-<name>.json` in `<dir>` for every `*.sql` file in `<src>` that
has a `.toml` file beside it. `<name>` is the `name` key in the `.toml`, not the
file name. A `.sql` file with no `.toml` is not built, and a
warning names it.

## libraries sql

```
fhir-pipes libraries sql <library> [--sources <source>...] [--warehouse <dir>] [--strip-prefix <prefix>...]
```

Prints the DuckDB SQL that runs one Library over a warehouse, to paste into
`duckdb`. `<library>` is a Library JSON file or a canonical URL (`url` or
`url|version`).

Options: `--sources` (see [Sources](#sources)), `--warehouse` (default `.`;
inputs are read from `parquet/<table>/`), and `--strip-prefix`, as the
config's `tables.strip_prefix`.

## Sources

A source is a directory of JSON files, a FHIR package `.tgz` (path or https
URL; https is downloaded on every run), or `name#version` from
`~/.fhir/packages`.

A source's SQL is code that fhir-pipes runs. Use only sources you trust.
Whoever controls an `https://` package's URL decides the SQL the next refresh
runs: pin it to a URL you control, or use a local `.tgz`. A Library's SQL must
be one statement. This does not make an untrusted Library safe to run.

A canonical resolves from the built-ins, then (for `refresh`) the views
directory, then each source in order. The first match wins for the same URL and version. A canonical with several versions needs an explicit `|version`, and a built-in cannot be redefined. A missing
dependency or a cycle stops the step before any SQL runs.

## Built-ins

`https://healthcampaigns.org/fhir/analytics/ViewDefinition/Location` is the
input of `https://healthcampaigns.org/fhir/analytics/Library/LocationHierarchy`,
a SQLView of each Location with its place in the `partOf` tree.

### LocationHierarchy

A Library that depends on `LocationHierarchy` reads the `location_hierarchy`
table that the `locations` step writes (see
[locations.md](locations.md#the-location_hierarchy-table)). If the table is
missing, the step stops and says to run the locations step.

## Where results go

Libraries run in dependency order, and a SQLView is inlined into each query
that reads it. A SQLQuery writes `parquet/<table>/`, partitioned by `country`
when the result has that column.

- With a `[database]` section, the `database` step loads each SQLQuery result
  as a table and each SQLView (not the built-in) as a PostgreSQL view. The
  result of a SQLQuery removed from the sources stays in the database as a
  table; drop it by hand. See [Database](refresh.md#database).
