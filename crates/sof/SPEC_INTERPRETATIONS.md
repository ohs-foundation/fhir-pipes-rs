# How fhir-pipes-sof reads the parts of the specs that are open

Some things the SQL on FHIR and FHIRPath specs leave to the implementation,
or say in a way that can be read more than one way. This page records what
fhir-pipes-sof does in each case and why, so a view's output can be predicted
and compared with other implementations. It describes the in-memory
evaluator. The SQL paths (DuckDB, PostgreSQL) pass the same conformance suite.

The tests that fhir-pipes fails, and why, are listed in
`tests/fhirpath/known-failures.json` (HL7 FHIRPath suite) and
`tests/fhirpath-js/known-failures.json` (fhirpath.js's tests); see
[Conformance](README.md#fhirpath).

## Resource and reference keys

The spec asks only that `getReferenceKey()` on a reference returns the same
value as `getResourceKey()` on the resource it points to. The form of the key
is left to the implementation.

**`getResourceKey()`** is the resource's `id`: `o1`, not `Observation/o1`.
Pathling uses `Observation/o1`. So keys of different resource types can be
equal (`Patient/1` and `Practitioner/1` are both `1`). To join only to one
type, pass the type: `subject.getReferenceKey(Patient)`.

**`getReferenceKey()`** reads `Reference.reference`:

| Reference | Key |
| --- | --- |
| `Patient/123` | `123` |
| `Patient/123/_history/2` | `123` (the version is not checked) |
| `#p1` | `p1`, the id of the contained resource it names |
| `http://example.org/fhir/Patient/123` | none |
| `urn:uuid:…` | none |
| no `reference` (identifier only) | none |

With a type argument, a key is given only for a reference to that type. For
`#p1` the type is the contained resource's `resourceType`, or else
`Reference.type`.

An absolute reference gets no key, even when it points to a resource in the
same data. It can name a resource on another server with the same id, and a
key for it could join to the wrong resource.

## `%rowIndex`

- At the top level of a view, outside any `forEach`, `%rowIndex` is `0`.
- Under `forEach` and `forEachOrNull` it is the 0-based position of the
  element in the collection the expression returned.
- In the row `forEachOrNull` adds when there is nothing to iterate,
  `%rowIndex` is `0`. The official suite expects this (`row_index.json`,
  "%rowIndex with forEachOrNull"). Pathling's own tests expect null.
- Under `repeat` it is the element's position in a depth-first walk in
  document order: an element, then its children, then its next sibling. The
  official suite expects this ("%rowIndex with repeat": `1`, `1.1`, `1.2`,
  `2`). Pathling's own tests number the elements breadth-first.

## `%context`, `%resource` and `%rootResource`

`%resource` and `%rootResource` are the view's resource everywhere in the
view, including inside `forEach`. `%context` is the node the expression
started from: the resource at the top level, and the current element inside
`forEach`, `forEachOrNull` and `repeat`. octofhir-fhirpath binds its own
`%context` to the resource throughout, so fhir-pipes renames the variable and
binds it per focus.

## `join()`

`join()` with no argument joins with no separator, as the FHIRPath spec says.
On an empty collection it returns empty, so the column is null, not `''`.
Both follow the SQL on FHIR suite.

## Values written to columns

- **Dates and times from the resource** are written as the resource wrote
  them. `2026-01-02T03:04:05Z` stays `2026-01-02T03:04:05Z`. fhir-pipes gives
  these elements the FHIRPath type String, which keeps their form; see
  [FHIR model](README.md#fhir-model). Comparisons and boundaries still read
  them as dates. Date arithmetic on them (`birthDate + 1 day`) does not work.
- **Dates and times the engine computes** (literals, arithmetic,
  `toDateTime()`) are in the engine's form: `@2020-01-01T10:00:00Z` is
  written `2020-01-01T10:00:00+00:00`. A time is written without the `T`,
  as FHIR writes it (`10:00`).
- **Decimals** are JSON numbers. Trailing zeros in the resource are not kept:
  `1.50` is written `1.5`, because the JSON is read into binary floating
  point. A decimal the engine computes keeps a decimal point (`1.5 * 2` is
  `3.0`).
- **Quantity elements** (`Quantity`, `Age`, `Duration` and the rest) have no
  FHIR type in the engine, so `comparator`, `id` and extensions can still be
  read; see [FHIR model](README.md#fhir-model).

## Elements the FHIR model does not type

The engine's FHIR model is the union of the R4 and R5 core types. An element
whose type differs between R4 and R5, or that neither version has (an R6
addition, for example), is navigated as plain JSON, with no type. A view
therefore runs the same on R4 and R5 data.

## `ofType()` on a choice element

`value.ofType(Quantity)` reads `valueQuantity`. A path is checked against the
FHIR model to see whether it reaches a choice element. See
[Known limitations](README.md#known-limitations) for where the path cannot be
followed.

On an element that is not a choice element, `ofType()` with a resource type
keeps the items of that `resourceType` (`contained.ofType(Practitioner)`), and
with a datatype keeps the items unchanged (`name.ofType(HumanName)`). The SQL
paths follow the element's owner type from the view's resource. Below a
`forEach`, where they do not know the items' type, they treat an element as a
choice element when any FHIR type has a choice element of that name.

## `repeat` paths

The spec applies a `repeat` path again to everything it returns, until
nothing new is found. It says nothing about a path that returns its own input
(`$this`), the resource (`%resource`), or a literal: applied again, each
returns what it was given, and the traversal never ends. A `repeat` path must
read child elements of the node it is applied to (`item`, `answer.item`,
`item.where(type = 'group')`, or a union of such paths). Each step then goes
one level down the resource, so the traversal ends. Both engines refuse other
paths when the view is compiled or its SQL generated, and the in-memory
engine also stops with an error past 128 levels.

## FHIRPath functions that need a type checker

The FHIRPath spec wants some expressions rejected before evaluation because of
their types: `name.given1` on a Patient, which has no such element, or
`@1974-12-25 + 7`, a date plus a number. octofhir-fhirpath has no type
checker, so it evaluates them, and the result is often empty. These are the
`type-checker` entries in the known-failures lists.
