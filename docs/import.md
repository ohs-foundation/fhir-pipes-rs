# Import packages

`fhir-pipes import run` turns a folder of non-FHIR records into FHIR NDJSON
for `fhir-pipes load`. How the records become resources is set by an
**import package**: the logical models of the records, the FHIR Mapping
Language maps that turn them into resources, and, where records have to be
joined or grouped first, ViewDefinitions and SQLQuery Libraries. The command
knows nothing about the system the records came from.

```sh
fhir-pipes import check --package examples/import/digit-visits --in records/
fhir-pipes import run   --package examples/import/digit-visits --in records/ --out fhir/
fhir-pipes import test  --package examples/import/digit-visits
```

[examples/import/digit-visits](../examples/import/digit-visits) uses every
feature described here, on synthetic data.
[examples/import/icr-dhis2](../examples/import/icr-dhis2) is the DHIS2
import as a package: three record types, each through its own map, with a
manifest that has no steps and no views or Libraries. Its records are what `fhir-pipes dhis2
extract locations|measures|reports` writes (see
[dhis2.md](dhis2.md#the-records)).

A program that writes such a folder from a source system is a connector.
Any program in any language can be one; see [connectors.md](connectors.md).

The same `import` command also has `location-boundaries` and
`location-sites`, which are unrelated to packages; see
[locations.md](locations.md).

## How an import works

```
source system ──connector──▶ records (NDJSON) ──import run + package──▶ FHIR NDJSON ──load──▶ FHIR server
```

1. A connector reads the source system and writes a folder of records, one
   NDJSON file per record type. It does no mapping: a record has the
   source's own fields. See [connectors.md](connectors.md).
2. `import check --in` checks each record against the package's logical
   model of its type, and type-checks the maps against the models. A field
   that the source renamed is reported here, before anything is mapped.
3. `import run` runs the package's steps over the folder:
   - a record that maps on its own goes through one map and becomes one or
     more resources;
   - records that have to be joined or grouped first are flattened by
     ViewDefinitions and joined by a SQLQuery Library, and each row of the
     result then goes through a map.
4. The output is one NDJSON file per resource type and a run report.
   `fhir-pipes load` sends the files to a FHIR server.

The package is the only place the mapping lives. `import run` has no code
for any source system, and a connector has none for FHIR. To change how a
field is mapped, change the package and run `import test`.

## Writing a package

In the order that usually works:

1. **Get sample records.** Run the connector, or write a few records by
   hand, into `tests/sample/in/<Type>.ndjson`. Do not put real personal
   data in a test case.
2. **Write a logical model for each record type** in `models/`: a
   StructureDefinition of kind `logical` with a snapshot, listing each
   field and its type. `import check --in` then reports every record that
   does not fit.
3. **Write a map for each record type** in `maps/`, from the logical model
   to FHIR resources ([maps.md](maps.md)). With one map per type, no
   `import.toml` is needed: see
   [Without a manifest, or without steps](#without-a-manifest-or-without-steps).
4. **Add steps where records have to be joined or grouped.** Either stage
   the records as FHIR resources and flatten them with views
   ([Steps](#steps)), or let a view read the record file directly
   ([Record views](#record-views)). A SQLQuery Library joins the views, and
   a map reads each row of its result
   ([A Library result as records](#a-library-result-as-records)).
5. **Declare parameters** for the values that differ between deployments:
   id prefixes, identifier system URLs, tags ([Parameters](#parameters)).
6. **Add data tests** on the Library results, such as a unique key
   ([Data tests](#data-tests)).
7. **Write the expected output.** `import test --update` writes
   `tests/sample/expected/` from the run. Read it, correct the maps until
   it is right, and commit it. `import test` then fails when a change to
   the package changes the output ([import test](#import-test)).

[examples/import/icr-dhis2](../examples/import/icr-dhis2) stops at step 3,
with parameters. [examples/import/digit-visits](../examples/import/digit-visits)
uses every step.

## The input folder

`--in` is a folder with one file per record type:

- named after the type: `DIGITTask.ndjson`, or `DIGITTask.ndjson.gz` for
  gzip (not both);
- one record per line, each a JSON object that is an instance of the
  package's logical model of that type.

A record's type is the last path segment of its logical model's URL
(`https://example.org/StructureDefinition/DIGITTask` is `DIGITTask`). A
record with no `resourceType` is given its type; one with another
`resourceType` is an error. Other files and subfolders are ignored. Every
record file must be read by a step. A step whose file is missing reads no
records; the run report says so.

Whatever writes such a folder can feed the import: Rust code in fhir-pipes,
an OpenFn workflow, a script or a database export.

## A package

A package is a folder:

| Path | What it holds |
|---|---|
| `import.toml` | The manifest: the steps, the data tests and what `--previous` does not compare. Optional |
| `models/` | StructureDefinition JSON with a snapshot: the logical models of the record types |
| `maps/` | `*.map` (FML), StructureMap JSON and ConceptMap JSON |
| `views/` | ViewDefinition JSON. Optional |
| `libraries/` | SQLQuery and SQLView Library sources, `NAME.sql` with `NAME.toml`, as [libraries build](libraries.md#writing-a-library) reads them. Optional |
| `tests/` | Test cases for `import test`. Optional |

`models/` and `maps/` are read together as one folder of maps (see
[maps.md](maps.md#a-folder-of-maps)), with the FHIR R4 core definitions built
in. The DHIS2 models built into `map run` are not: the import reads the
models in the package's `models/`, so
[examples/import/icr-dhis2](../examples/import/icr-dhis2) uses its own copies.

### Without a manifest, or without steps

With no `import.toml`, or one with no `[[step]]`, each record file is run
through the one map in `maps/` whose first group reads its type, and the
resources go to the output. A file whose type has no model, no such map or
more than one is an error. With no `import.toml`, the package is named after
its folder; a manifest with no steps still gives the name and can have a
`[compare]` section. [examples/import/icr-dhis2](../examples/import/icr-dhis2)
has such a manifest:

```toml
name = "icr-dhis2"

[compare]
ignore = ["MeasureReport.date"]
```

## import.toml

```toml
name = "icr-digit"
title = "DIGIT to the Integrated Campaign Registry"

[[step]]                    # records through a map, resources to the output
map = "DIGITFacilityToICR"
from = "DIGITFacility"

[[step]]                    # the same, but the resources are staged for the views
map = "StageDIGITTask"
from = "DIGITTask"
stage = true

[[step]]                    # a SQLQuery Library, with the views it reads
library = "DigitVisit"

[[step]]                    # each row of the Library's result through a map
map = "DIGITVisitToICR"
from = { library = "DigitVisit" }

[[test]]
name = "one row per visit"
library = "DigitVisit"
unique = ["visitKey"]
not_null = ["householdId", "workerId"]

[[test]]
name = "every staged task is in a visit"
sql = "SELECT id FROM stg_task EXCEPT SELECT u.t.crid FROM digit_visit, unnest(task) AS u(t)"
```

| Key | Meaning |
|---|---|
| `name` | The package's name. Required |
| `title` | A title. Optional |
| `[[step]]` | The steps, run in the order given |
| `[[test]]` | Data tests on the Library results |
| `[records]` | How records with nulls or members their model does not describe are read. Optional |
| `[maps]` | The limit on the values a map may create from one record. Optional |
| `[views]` | Whether a view may read a record file directly. Optional |
| `[compare]` | What `import run --previous` does not compare. Optional |
| `[parameters]` | Settings a run can change, with their defaults. Optional. See [Parameters](#parameters) |

Any other key, here or in a step, test, `[records]`, `[maps]`, `[views]` or `[compare]`, is an error.

### Steps

A step is one of:

| Keys | What it does |
|---|---|
| `map`, `from = "Type"` | Runs the map over every record of `<in>/<Type>.ndjson`. The map's first group must read that type's model |
| `map`, `from = { library = "L" }` | Runs the map over every row of Library `L`'s result. An earlier library step must run `L` |
| `stage = true` (with `map`) | The resources are staged for the views instead of written to the output |
| `library = "L"` | Runs SQLQuery Library `L` |

`map` and `library` take a name or a canonical URL. `library` also takes
`url|version`.

Every stage step must come before the first library step, because the views
read only what is staged before them. Steps that write to the output can
come anywhere.

A library step runs the views and SQLQuery Libraries that `L` depends on,
through any SQLView it reads, and then `L`. A view or Library that an
earlier step ran is not run again, and one that no step needs is not run.
A view over a FHIR resource type reads only the staged resources. A view
of a type nothing staged gives an empty table. A view over a record type
reads the record file instead (see [Record views](#record-views)). A Library cannot depend on the built-in Location view
or LocationHierarchy: the import has no Location tables.

### A Library result as records

Each row of a Library's result becomes one record of the model the map
reads:

- each column is a field, named exactly as the SQL names it. Alias the
  columns to the model's element names, quoted to keep their case
  (`round_id AS "roundId"`);
- a `list` column is a JSON array and a `struct` is a JSON object, nested to
  any depth;
- a SQL NULL is left out, at any depth;
- `resourceType` is set to the model's type;
- dates and times are the strings the SQL produces. Use `strftime` to choose
  the format; a TIMESTAMPTZ left as it is comes out as DuckDB writes it in
  UTC (`2026-07-01 09:00:00+00`), which is not a FHIR dateTime.

The rows are read in the order of the result. Give the Library an
`ORDER BY` so the output is in the same order on every run.

The rows are checked against the model as records are (below), so a column
named differently from its element is reported before the map runs.

### Data tests

A test is one of:

| Keys | Fails when |
|---|---|
| `library`, `unique = [columns]` | Two rows of `library`'s result share a non-null value in a column. Each column is checked on its own |
| `library`, `not_null = [columns]` | A row has a null in a column |
| `sql` | The query returns rows |

`name` is optional and names the test in messages. A test with both
`unique` and `not_null` reports each column as a check of its own.

A `sql` test is one query. It reads tables by their warehouse names: each
view or Library run so far, named from its `name` in snake case
(`StgTask` is `stg_task`, `DigitVisit` is `digit_visit`), as
[refresh](refresh.md#libraries) names them. These are not the `depends-on`
labels, which belong to each Library. A manifest with tests must have a
library step.

All the tests run once, after the last library step and before the steps
after it. Every test runs, and every failure is reported with its count and
first rows. If any fails, the run stops with exit 1 and writes nothing to
`--out`: not even the output of steps that ran before the tests.

### Records

```toml
[records]
null = "absent"
unknown = "ignore"
```

| Key | Meaning |
|---|---|
| `null` | `error` (the default): a null value is a problem. `absent`: a member whose value is null is read as if it were not there, at any depth. A null item of an array is still a problem |
| `unknown` | `error` (the default): a member the model does not describe is a problem. `ignore`: it is left out of the record before the record is checked and mapped, at any depth the model describes |

Records that a system's API returns as they are, such as DIGIT search
results, set most members to null when they have no value, and carry
members a mapping has no use for: audit details, row versions, free-form
`additionalDetails` objects whose keys no model can list. With both set,
the model describes only what the package reads, and a record is read as
an instance of it.

The cost is that a member renamed at the source is no longer reported, but
left out: make the elements the maps need `min` 1 where the source always
has them, so a rename is still found. `import check --in` names the members
it left out, with the number of records that had each, so what is ignored
can be read.

`[records]` applies to the record files of `--in`. The rows of a Library
result are always read with SQL NULLs left out and every column checked
against the model.

### Maps

```toml
[maps]
max_values = 20_000_000
```

| Key | Meaning |
|---|---|
| `max_values` | The values one transform may create: the elements it writes and the values it works with. Default 2,000,000 |

The limit stops a map that runs away, such as a group that calls itself
without end. A record that is a large tree, such as a DIGIT boundary
hierarchy with 80,000 boundaries in one record, needs more: its map writes
every Location in one transform. A run past the limit stops with
`the transform created more than 2000000 values`, and says where to raise it.
Memory grows with the values: the 83,194 Locations of one DIGIT hierarchy
took about 2.5 GB.

### Views

```toml
[views]
records = "allow"
```

| Key | Meaning |
|---|---|
| `records` | `error` (the default): a view whose `resource` is a record type is refused, naming the view and this setting. `allow`: such a view reads the record file of its type (see [Record views](#record-views)) |

## Parameters

A package can take settings that would otherwise be written into its maps,
views and SQL: an id prefix, a system URL, a time window. They are declared
under `[parameters]` with their defaults, and a run can change them with
`--param NAME=VALUE`.

```toml
[parameters]
idPrefix = "dgt-"
visitGapSeconds = { default = 7200, description = "A visit ends when more than this many seconds pass between two tasks" }
asOf = { type = "string", description = "When the records were extracted" }
```

Each key is a parameter. Its value is either the default, or a table with:

| Key | Meaning |
|---|---|
| `default` | The default value. Without it, every run must give a value |
| `type` | `string`, `integer`, `decimal` or `boolean`. Needed without `default`; otherwise taken from it. An integer default of a `decimal` parameter is read as a decimal |
| `description` | One line saying what the parameter is for |

A name is letters, digits and `_`, and does not start with a digit. The
names FHIRPath and the view engine already use (`context`, `resource`,
`rootResource`, `ucum`, `sct`, `loinc`, `factory`, `terminologies`,
`server`, `recordLine`, `rowIndex`) are refused. A value is a string, an
integer, a decimal or a boolean; dates and times are strings.

### Giving values

`import check`, `import run` and `import test` take `--param NAME=VALUE`,
repeatable. The value is read as the parameter's type (`--param
visitGapSeconds=3600`, `--param strict=true`). A name the package does not
declare, a value that is not of the type, or one name given twice is an
error (exit 2). A parameter with no default and no `--param` stops `import
run` and `import test`; `import check` lists it as having no value.

A test case of `import test` can set values in `tests/<case>/parameters.toml`,
one `NAME = value` per line, typed as in TOML:

```toml
visitGapSeconds = 86400
```

The values a case runs with are the defaults, then `--param`, then the
case's file: `--param` applies to every case, and a case's file wins over
it, so a case keeps the values its `expected/` was made with.

The run report has the value of every parameter under `parameters`:

```json
"parameters": {"idPrefix": "dgt-", "stagingTag": "digit-staging", "visitGapSeconds": 7200}
```

### In maps

A map reads a parameter as the FHIRPath environment variable `%NAME`, in
any expression it evaluates: `where`, `check`, `log`, `evaluate(...)` and a
`let` constant. A transform such as `append` or a plain copy takes a
variable, not an expression, so give the parameter a `let` constant of the
same name first:

```
let idPrefix = %idPrefix;
let stagingTag = %stagingTag;

group StageTask(source src : DIGITTask, target tk : Task) {
  src -> tk.meta as m, m.tag as tag, tag.code = stagingTag "tag";
  src.roundId as r -> tk.basedOn as ref, ref.reference = append('CarePlan/', idPrefix, r) "round";
}
```

In an expression the parameter is read directly:

```
  src -> tk.id = evaluate(src, visitKey.replace('stg-', %idPrefix + 'visit-')) "id";
```

A variable or constant of the map with the same name hides the parameter,
except in its own `let` (`let idPrefix = %idPrefix;` reads the parameter).
A `%NAME` that is neither stops the transform with `Unknown variable`.
ConceptMaps cannot read parameters.

`map run` runs a package's maps outside `import run`: it gives the maps the
defaults in `import.toml` of the folder given with `--maps`, or of the
folder above it (the package of a `maps/` folder), and has no `--param`. A
parameter with no default is an error there.

`dhis2 locations|measures|reports --package DIR` load the package as
`import run` does, and take `--param NAME=VALUE` as it does (see
[dhis2.md](dhis2.md#the-commands-that-write-resources)).

### In views

Every view the run runs, over staged resources or a record view, gets each
parameter as a ViewDefinition constant, so its paths read it as `%NAME`, as
they read the view's own `constant` values:

```json
"where": [{"path": "meta.tag.where(code = %stagingTag).exists()"}]
```

A view may not declare a constant with a parameter's name (exit 2 when the
package is loaded).

### In SQLQuery Libraries

A Library declares the parameters it reads under `[[parameter]]` in its
`.toml`, as [libraries build](libraries.md#writing-a-library) reads them,
and writes each as `:NAME` in its SQL, as the SQL on FHIR SQLQuery profile
does:

```toml
[[parameter]]
name = "visitGapSeconds"
type = "integer"
```

```sql
SELECT ... CASE WHEN gap IS NULL OR gap > :visitGapSeconds THEN 1 ELSE 0 END ...
```

Each parameter a Library declares must be a parameter of the package with
the same name, and its `type` one the package's value can be cast to; both
are checked when the package is loaded (exit 2). The value is not written
into the SQL: each `:NAME` becomes `getvariable('NAME')`, and the script
sets that variable with `SET VARIABLE NAME = CAST(getenv('FHIR_PIPES_PARAM_NAME') AS TYPE)`,
the value passed to duckdb in its environment. This needs duckdb 1.1 or
later.

| Library `type` | duckdb type | Package parameter |
|---|---|---|
| `string`, `code`, `id`, `uri`, `url`, `canonical`, `markdown`, `oid`, `uuid` | `VARCHAR` | string |
| `date`, `dateTime`, `instant`, `time` | `DATE`, `TIMESTAMPTZ`, `TIME` | string |
| `integer`, `positiveInt`, `unsignedInt`, `integer64` | `BIGINT` | integer |
| `decimal` | `DOUBLE` | decimal or integer |
| `boolean` | `BOOLEAN` | boolean |

A `:NAME` is found only in the code of the Library's own SQL, not in a
string, a quoted name or a comment, and `::` is a cast. Only the names the
Library declares are replaced; any other `:NAME` is left for duckdb. A
SQLView cannot declare parameters, as before, and the SQL of `sql` data
tests does not take them.

### What import check reports

| Finding | |
|---|---|
| A map or view reads `%NAME`, and `NAME` is neither a parameter nor the map's variable or the view's constant | error. FHIRPath's own names (`%resource`, `%ucum`, `%vs-…` and the like) and `%recordLine` in a record view are not reported |
| A parameter that no map or view reads as `%NAME` and no Library declares | warning |
| A Library declares a parameter and its SQL has no `:NAME` | warning |
| A Library's SQL has `:NAME` for a package parameter it does not declare, so it gets no value | warning |

It also lists each parameter with the value a run would use.

## Record views

A *record view* is a ViewDefinition in `views/` whose `resource` is one of
the package's record types, the name of a logical model in `models/`
(`DIGITFacility`), not a FHIR resource type. It reads the record file of
that type from `--in`, not staged resources.

The SQL on FHIR specification binds `ViewDefinition.resource` to the FHIR
resource types, so a record view goes beyond it, and a package has to allow
record views with `[views] records = "allow"`. The standard form remains
available: stage the records with a map (`stage = true`) and write the view
over the staged resources. [examples/import/digit-visits](../examples/import/digit-visits)
uses that form.

```json
{"resourceType": "ViewDefinition", "name": "Facility", "status": "draft",
 "resource": "DIGITFacility",
 "select": [{"column": [
   {"name": "line", "path": "%recordLine", "type": "integer"},
   {"name": "id", "path": "id", "type": "string"},
   {"name": "locality", "path": "address.locality.code", "type": "string"}]}]}
```

### What a record view reads

- The records of its type as a map step reads them: with the
  [`[records]`](#records) rules applied, then checked against the model. A
  record that does not fit stops the run with exit 2, naming the file and
  line, as in a map step.
- A missing record file gives an empty table, as a type nothing staged
  does.
- The view runs when a Library that depends on it runs, as any view does.
  Record views of one type that one library step runs share one read of the
  file, a chunk of lines at a time; the records are not held in memory, only
  the view's rows, as for any view.
- Its table is named as any view's (`Facility` is `facility`), so Libraries
  and `sql` data tests read it the same way.
- A view over a FHIR resource type still reads the staged resources. Both
  kinds can be in one package, and one Library can read both.

### Keys

A record is not a resource and has no key of its own.

| Function | In a record view |
|---|---|
| `getResourceKey()` | Refused when the package is loaded (exit 2), naming the view and the path. Select the field that identifies the record by its path (`id`) |
| `getReferenceKey()` | Allowed. It reads a `Reference` value and works as in any view |

Without the refusal, the view engine would return the record's `id` member
where it has one, and on an element inside the record, that element's `id`.

### The record's line

`%recordLine` is the record's line in its file, counted from 1: the line
number that `import check` and the run's messages give (`file:line`). Blank
lines are counted. It is the same throughout the view, inside `forEach` and
`repeat` too. Only a record view has it: a view over a resource type that
uses it is refused when the package is loaded.

The rows of a view, and so of the tables, come in the order of the records.
SQL keeps no order, so a Library that wants the records' order selects the
line and sorts by it (`ORDER BY line`).

### What import check verifies

`import check` lists the record views on one line:

```
record views, which read record files directly, beyond the SQL on FHIR specification: Boundary (DIGITBoundaryTree), Facility (DIGITFacility)
```

It checks the paths of each record view against the model of its type, and
reports an element the model does not have as an error, with the view, the
path and the model:

```
error: view Facility (DIGITFacility, https://icr.healthcampaigns.org/StructureDefinition/DIGITFacility): column `name`, path `nmae`: unknown element `nmae` — did you mean `name`?
```

| Checked | Not checked |
|---|---|
| Each `column.path`, `forEach`, `forEachOrNull` and `where` path that is a chain of element names, from the record or from the `forEach` items above it | Anything else: operators (`a = 'x'`, `a and b`), unions, literals, variables (`%recordLine`) |
| Through `first()`, `last()`, `single()`, `tail()`, an index (`[0]`) and `where(...)` | The arguments of a function, such as the condition inside `where(...)`. The walk stops at any other function: what follows `exists()`, `empty()` or `ofType()` is not checked |
| Into a BackboneElement, an element with a `contentReference`, an element typed with another model of the package, and a FHIR complex type (`CodeableConcept`, by the FHIR R4 and R5 core types) | |
| The columns under `repeat`, when every node it reaches has the same elements (a tree whose children element refers back to its parent with `contentReference`) | The columns under a `repeat` whose nodes differ, or whose paths are not chains of element names |
| `id` and `resourceType` at the top of a record | |

A path it cannot follow is not reported. The check runs in `import check`
only; `import run` and `import test` do not run it.

`import check` also warns where the view engine can read one element for
another. When an object has no member `a`, the engine looks for a member
named `a` and then a capital letter, as it would for a choice element
`a[x]`, and returns that. So where a model has elements `name` and
`nameLocal`, a record with no `name` gives `nameLocal`'s value for a path
that reads `name`:

```
warning: view Shop (Shop): Shop has elements `name` and `nameLocal`: in a record with no `name`, the view engine reads `nameLocal` for a path that reads `name`, as if `name` were a choice element
```

The same happens in a `repeat` whose paths are applied to nodes that do not
have them: `repeat: ["boundary", "children"]` over a DIGIT boundary tree
reads `boundary` on each boundary, which has no such member, as its
`boundaryType`. Use `forEach` with a `unionAll` instead:

```json
"select": [{"forEach": "boundary", "unionAll": [
  {"column": [{"name": "code", "path": "code"}]},
  {"repeat": ["children"], "column": [{"name": "code", "path": "code"}]}]}]
```

### Names

A logical model whose type is also a FHIR type (`Location`, `HumanName`) is
refused when the package is loaded: a view over it, or a record whose
`resourceType` names it, could be either.

### Compare

```toml
[compare]
ignore = ["MeasureReport.date", "Observation.meta.tag"]
```

| Key | Meaning |
|---|---|
| `ignore` | Elements that [`import run --previous`](#running-an-import-repeatedly) does not compare, as `ResourceType.path` |

A path is a resource type, then one or more element names separated by
dots: a top-level element (`MeasureReport.date`) or an element inside
objects (`Observation.meta.tag`). Arrays are not indexed: a path that goes
through an array applies to every item of it (`MeasureReport.group.code`
is the `code` of every group). The element is left out with everything
under it, for that resource type only. `id` and `resourceType` cannot be
left out. A path that does not fit this syntax is an error when the
manifest is read.

Use it for an element that changes on every run without the content
changing. In the DHIS2 package, a MeasureReport's `date` is when DHIS2 last
built its analytics tables, which changes every day.

## import check

```
fhir-pipes import check --package DIR [--in RAW] [--param NAME=VALUE ...]
```

Loads the package and reports:

- syntax and type errors in the maps, as [map check](maps.md#map-check)
  does;
- problems with the manifest: a map, model, Library or view that does not
  resolve, a step out of order, a test of a Library no step runs, a record
  view the manifest does not allow. These stop the check with exit 2;
- the record views, on one line, and the paths in them that name an
  element their model does not have (see
  [What import check verifies](#what-import-check-verifies));
- the parameters and their values, and what is wrong in their use (see
  [What import check reports](#what-import-check-reports)).

With `--in`, it also checks every record of every file against its logical
model, and reports a file that no step or record view reads. Each problem is printed as
`file:line: message`, at most 20 per file, then a count of the rest. A
record is checked against its model's snapshot:

| Check | Example message |
|---|---|
| Every key is an element. `resourceType` and `id` are always allowed, and `_name` beside an element `name` | ``unknown element `colour` `` |
| An element with `min` 1 or more is present (an empty array counts as absent) | ``missing required element `roundId` `` |
| An element with `max` 1 has one value | `` `roundId` is an array, but takes one value `` |
| A repeating element has a JSON array, as FHIR JSON requires, even for one value | `` `task` repeats, so its value must be a JSON array `` |
| No value is null | `` `task[1]` is null `` |
| A primitive has its JSON type: string-like types a string, `boolean` a boolean, `integer`, `positiveInt` and `unsignedInt` a whole number in range, `decimal` a number | `` `count` must be a whole JSON number (integer), not a string `` |
| A BackboneElement, a `contentReference`, and an element typed with another model of the package are checked inside the same way | ``missing required element `task[0].crid` `` |

With [`[records]`](#records), nulls and the members the model does not
describe are taken out first, as the run does, and the check prints one line
per file naming the members it left out:

```
in/DIGITFacility.ndjson: ignored 16 members the model does not describe (records with each): additionalFields (75), address.addressLine1 (75), ...
```

Choice elements (`value[x]`) are matched by their type suffix
(`valueString`). A FHIR complex type such as CodeableConcept is checked only
to be a JSON object; its content is not checked. String formats (dates,
codes) are not checked.

Exits 0 when clean, 1 when it printed findings, 2 for a problem with the
package or the options.

## import run

```
fhir-pipes import run --package DIR --in RAW --out OUT [--threads N] [--report FILE] [--overwrite] [--keep-work DIR]
                      [--previous DIR [--changed DIR] [--removed FILE]] [--param NAME=VALUE ...]
```

| Option | Meaning |
|---|---|
| `--package DIR` | The package folder |
| `--in RAW` | The record files (see [the input folder](#the-input-folder)) |
| `--out OUT` | The output folder: one `<ResourceType>.ndjson` per resource type. Must be empty or not exist |
| `--overwrite` | Allow a non-empty `--out`: replace the files this run writes, and remove the `*.ndjson` files the last run there wrote that this run does not |
| `--threads N` | Threads that run the maps and views. Default: one per core. The output files are the same, byte for byte, at any count |
| `--report FILE` | Write the run report, JSON, to this file |
| `--keep-work DIR` | Keep the intermediate files in `DIR`, which must be empty or not exist |
| `--previous DIR` | The `--out` folder of an earlier run of the same package. This run's resources are compared with it. See [Running an import repeatedly](#running-an-import-repeatedly) |
| `--changed DIR` | Requires `--previous`. Gets the resources that are new or differ from the previous run, one `<ResourceType>.ndjson` per type, in the same order as in `--out`. A type with nothing changed gets no file. Must be empty or not exist, unless `--overwrite` |
| `--param NAME=VALUE` | The value of a package parameter, instead of its default. Repeatable. See [Parameters](#parameters) |
| `--removed FILE` | Requires `--previous`. Gets one `ResourceType/id` per line, sorted, for every resource of the previous run that this run did not write. Replaced if it exists |

Each record is read by the package's [`[records]`](#records) rules and
checked against its model (as `import check --in` does) before its map runs. The first record that does not fit, or that its map
fails on, stops the run with exit 2 and names the file and line, or the
Library and row. Engine warnings, such as a code a ConceptMap has no target
for, do not stop the run: they are printed (the first 10 per step) and kept
in the report (the first 100 per step, with the exact count).

The output holds every resource the non-staged steps wrote, one file per
resource type, in step order and then record order. A map with a `Bundle`
target writes the resources of its entries. Nothing is written to `--out`
until every step and test has passed; the files are then moved in. A run
that fails in a step or a test leaves `--out` as it was, and `--changed`
and `--removed` too.

`--out` gets a file `.fhir-pipes-import.json` listing the files the run
wrote. With `--overwrite`, the next run removes only the files in that list
that it does not write again; it does not touch any other file.

The run needs the `duckdb` command on `PATH` when it has a library step.

### The work folder

The staged resources, the view tables, the Library results and the rows read
from them are written to a temporary folder beside `--out`
(`.fhir-pipes-import-*`), which is removed when the run ends, whether it
passed or failed. They can hold personal data: the folder is created
readable by its owner only, and every file in it is mode 0600 on Unix.

| File | What it holds |
|---|---|
| `staged/<ResourceType>.ndjson` | The resources of the stage steps. Record views read `--in`, not this |
| `tables/<table>.parquet` | Each view's rows and each Library's result |
| `rows/<table>.ndjson` | A Library's result as records, for a map |
| `out/<ResourceType>.ndjson` | The output, before it is moved to `--out` |

With `--keep-work DIR` the files are kept there instead, to open in DuckDB
or read when a map does something unexpected.

### The run report

```json
{
  "package": "digit-visits",
  "parameters": {"idPrefix": "dgt-", "stagingTag": "digit-staging", "visitGapSeconds": 7200},
  "status": "ok",
  "steps": [
    {"step": 1, "map": "DIGITFacilityToICR", "from": "DIGITFacility",
     "file": "records/DIGITFacility.ndjson", "records": 2,
     "resources": {"Location": 2}, "warnings": 1,
     "warning_list": [{"record_type": "DIGITFacility", "at": "records/DIGITFacility.ndjson:2",
                       "message": "rule `physicalType`: `Mobile` has no translation in ...; left out"}]},
    {"step": 5, "library": "DigitVisit",
     "tables": [{"table": "stg_task", "from": "view StgTask", "rows": 4},
                {"table": "digit_visit", "from": "Library DigitVisit", "rows": 3}]}
  ],
  "tests": [
    {"name": "one row per visit", "check": "unique visitKey in digit_visit",
     "passed": true, "failures": 0}
  ],
  "output": {"Location": 2, "MedicationAdministration": 4, "Task": 3}
}
```

With `--previous`, the report also has the comparison: the counts for every
resource type together, and by type, including the types only the previous
run had:

```json
"comparison": {
  "previous": "fhir",
  "new": 1, "changed": 1, "unchanged": 19, "removed": 1,
  "types": {
    "Location": {"new": 0, "changed": 0, "unchanged": 6, "removed": 0},
    "MeasureReport": {"new": 1, "changed": 1, "unchanged": 6, "removed": 1}
  }
}
```

`status` is `ok`, `failed` (a data test failed; `examples` holds the first
5 failing rows of each failed test) or `error` (with `error`, the message).
`file` is `null` for a step whose record file is missing. A table of a
record view has `file` too: the record file the view read, or `null`. The report is
written whatever the outcome once the steps have started; a problem found
before then (a bad package, a non-empty `--out`) is only printed.

### Running an import repeatedly

An import that runs every week mostly writes resources that have not
changed. With `--previous`, the run compares its resources with an earlier
run's, so only what changed needs loading, and what disappeared can be
deleted from the FHIR server:

```sh
# The first run.
fhir-pipes import run --package PKG --in raw --out fhir
# Every run after it: fhir/ is the baseline, read and then replaced.
fhir-pipes import run --package PKG --in raw --out fhir --overwrite \
  --previous fhir --changed fhir-changed --removed fhir-removed.txt
for f in fhir-changed/*.ndjson; do fhir-pipes load --server URL --in "$f"; done
```

- `--out` still gets every resource, so it is the baseline for the next run.
- `--previous` can be the `--out` folder when `--overwrite` is given: the
  previous resources are read before anything is replaced. Without
  `--overwrite` that is refused.
- A resource is identified by `resourceType` and `id`. With `--previous`,
  a resource with no `id`, in this run or the previous one, is an error that
  names the file and line (for this run, the record the map read).
- A resource differs when its JSON content differs. Key order does not
  matter. `meta.versionId`, `meta.lastUpdated` and `meta.source` are never
  compared, as a FHIR server sets them: `--previous` can be a folder of
  resources exported from the server. Nor are the elements in the
  package's [`[compare] ignore`](#compare). Numbers are compared as written,
  so `1` and `1.0` differ.
- The files read in `--previous` are those its `.fhir-pipes-import.json`
  lists, or with no such file, every `*.ndjson` and `*.ndjson.gz` file in
  it.
- `--changed` and `--removed` are written only when the run passes, and
  before `--out` is replaced. A resource written twice in a run is compared
  twice, and goes to `--changed` twice when it is new or changed.
- `--changed` keeps a `.fhir-pipes-import.json` as `--out` does, so with
  `--overwrite` the files of types that changed last time and not this
  time are removed from it.
- The files in `--changed` and the `--removed` file are readable by their
  owner only on Unix (mode 0600), as the files in `--out` are.
- Only the previous run's hashes are kept in memory: 16 bytes of key and 8
  of content hash per resource. The previous files are read twice.

`--removed` lists every resource of the previous run that this run did not
write, so it is only meaningful when both runs cover the same scope: the
same record files, from the same source selection. A run over a subset (one
district, the last month) lists the rest as removed. `dhis2 reports
--previous` handles this with a scope of indicators, org units and periods;
`import run` has no such scope.

## import test

```
fhir-pipes import test --package DIR [--case NAME] [--update] [--threads N] [--param NAME=VALUE ...]
```

A test case is a folder `tests/<case>/` with `in/`, record files as for
`--in`, and `expected/`, the `<ResourceType>.ndjson` files the run should
write. `import test` runs the package over each case's `in/` and compares the
output with `expected/`: the same files, and in each file the same resources
in the same order, compared as JSON values, so key order and spacing do not
matter.

A difference is printed with the case, the file, the line and the resource
(`Task/dgt-visit-t-004`), and each differing path with both values, at most 8
paths per resource and 10 resources per file. A case whose run fails, or
whose data tests fail, fails. The command exits 1 if any case fails.

A case can set parameter values in `tests/<case>/parameters.toml`, and
`--param` sets them for every case; see [Giving values](#giving-values).

`--case NAME` runs one case. `--update` writes each case's output to its
`expected/` folder instead of comparing, replacing the `*.ndjson` files
there; check the change before committing it.

## Limits

- Maps are read as FHIR R4.
- A map sees one record. Joins and grouping are done with views and
  Libraries, over staged resources or, with record views, over the record
  files.
- Views and Libraries run in DuckDB, one Parquet file per table. The import
  has no partitions, no Location tables and no PostgreSQL.
- A Library gets only the package's parameters as its parameters; `refresh`
  and `serve` still do not bind Library parameters.
- Records are checked against the snapshot only. Constraints, value set
  bindings and string formats are not checked.
- One record is one transform. A record that holds many resources' worth
  of data (a tree of boundaries) is mapped on one thread, and may need
  `[maps] max_values`.
- Each record file is read once per step that reads it, and once per
  library step that runs record views of its type; staged resources and
  Library results are written to disk, not held in memory. A view's rows
  are held in memory while it runs, as in `refresh`.
