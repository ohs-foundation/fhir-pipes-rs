# Maps

`fhir-pipes map` runs [FHIR Mapping Language](https://build.fhir.org/ig/HL7/mapping-language-ig/)
(FML) maps: a map turns a record described by a StructureDefinition into FHIR
resources. The record does not have to be FHIR. A logical model describes
its fields, and ConceptMaps translate its codes.

```sh
fhir-pipes map check --maps maps/
fhir-pipes map run   --maps maps/ --in records.ndjson --out resources.ndjson
```

`fhir-pipes dhis2 --maps` uses the same engine to write Locations, Measures
and MeasureReports; see [Maps for the DHIS2 import](#maps-for-the-dhis2-import).
To run several maps over a folder of records as one pipeline, with views and
SQL where records have to be joined, see [import.md](import.md).

## A folder of maps

`--maps` is a folder, read with its subfolders, and can be repeated. It holds:

| File | What it is |
|---|---|
| `*.map`, `*.fml` | A map, in FML |
| `StructureMap` JSON | A compiled map |
| `StructureDefinition` JSON | A logical model the maps read, or a profile or extension they write |
| `ConceptMap` JSON | A code translation, used by `translate()` in a map |

Other files are ignored. The FHIR R4 core definitions are built in, and so
are the logical models of the DHIS2 records (below).

A map names the types it reads and writes with `uses`, and its first group is
where it starts:

```
/// url = 'https://example.org/StructureMap/FacilityToLocation'
/// name = 'FacilityToLocation'

uses "https://example.org/StructureDefinition/Facility" alias Facility as source
uses "http://hl7.org/fhir/StructureDefinition/Location" alias Location as target

group Facility(source src : Facility, target loc : Location) {
  src.code as c -> loc.id = c "id";
  src.name as n -> loc.name = n "name";
  src where closed.exists() -> loc.status = 'inactive' "closed";
}
```

A map that writes more than one resource per record uses a `Bundle` as its
target and puts each resource in an entry.

### Writing a map that runs fast

Every FHIRPath expression in a map is parsed and evaluated each time its rule
runs: an `evaluate(...)` transform, a `where` or `check` condition. Native
transforms and copied variables are not. In the example maps, replacing
FHIRPath string joins with `append` roughly halved the run time.

- Join strings with `append`: `src.id as i -> ref.reference = append('Location/', i)`,
  not `evaluate(src, 'Location/' + id)`.
- Copy a field through a variable: `g.name as n -> c.display = n`, not
  `c.display = evaluate(g, name)`.
- Test a condition once and nest the rules that depend on it in
  `then { ... }`, instead of repeating the `where` on each rule.

## map run

Runs one map over every line of an NDJSON file.

| Option | Meaning |
|---|---|
| `--maps DIR` (repeatable) | The folders of maps, definitions and ConceptMaps |
| `--map NAME` | The map to run, by name or canonical URL. Not needed when the folders hold one map |
| `--in FILE` | NDJSON, one record per line. A record with no `resourceType` is read as the map's source type |
| `--out FILE` | NDJSON of the resources written |
| `--threads N` | Threads that run the map. Default: one per core. The output is the same, in input order, at any count |

For a `Bundle` target the output is the resources in its entries, one per
line; for any other target it is the target. A record the map fails on stops
the run, naming the line. A value the engine leaves out, such as a code a
ConceptMap has no target for, is printed as a warning with its line.

## map check

Reports syntax errors and type errors in every FML file of the folders: an
element that does not exist on the type it is read from or written to, an
unknown variable, a wrong type, a call to a group that does not exist. It
checks against the logical models and profiles in the folders, so a field
misspelt on either side of a rule is found before anything runs. It exits 1
when there are errors.

## Maps for the DHIS2 import

By default `fhir-pipes dhis2` writes its resources with a mapping built into
the binary, taking systems and profile URLs from the profile's `[dhis2]`
section. With `--maps` (or `$FHIR_PIPES_DHIS2_MAPS`, comma-separated) the
resources are written by maps instead, so the mapping can be read, changed
and published with an implementation guide without a new fhir-pipes release.

```sh
fhir-pipes dhis2 locations --dhis2 URL --instance sl-demo --id-prefix sle \
  --maps examples/import/icr-dhis2/maps --out sl-locations.ndjson
```

fhir-pipes still does everything a map cannot: it reads the DHIS2 API,
checks each record, and works out the values that depend on other records or
on the run. It hands the map one prepared record at a time. The records'
logical models are in
[examples/import/icr-dhis2/models](../examples/import/icr-dhis2/models), and
are built into fhir-pipes for `--maps`:

| Command | Record | What fhir-pipes prepares |
|---|---|---|
| `dhis2 locations` | [DHIS2OrgUnit](../examples/import/icr-dhis2/models/StructureDefinition-DHIS2OrgUnit.json) | The id prefix, facility or admin unit from the level, the parent only when it was exported, the boundary (merged from the children for a unit with none), the point, the org unit groups |
| `dhis2 measures` | [DHIS2Indicator](../examples/import/icr-dhis2/models/StructureDefinition-DHIS2Indicator.json) | Indicators, data elements and reporting rates in one shape, the date in the server's time zone |
| `dhis2 reports` | [DHIS2Value](../examples/import/icr-dhis2/models/StructureDefinition-DHIS2Value.json) | The report id, the Location the org unit maps to, the period's dates, counts as whole numbers, the score, and each breakdown with its options |

For each command the folders must hold exactly one map whose first group
reads that record type. The map for `dhis2 reports` must write one
MeasureReport with the record's `id`, and put the period extension first.

[examples/import/icr-dhis2/maps](../examples/import/icr-dhis2/maps) holds
the maps and ConceptMaps for the Integrated Campaign Registry. They write the
same resources as the built-in mapping with `examples/profiles/icr.toml`; the
tests run the DHIS2 import both ways and compare. Copy them to start a
mapping for another implementation guide.

The folder is part of an import package,
[examples/import/icr-dhis2](../examples/import/icr-dhis2). `dhis2 extract`
writes the prepared records instead of resources, and
`fhir-pipes import run` turns them into the same resources with these maps
(see [dhis2.md](dhis2.md#records-and-the-import-package)).

Notes:

- The profile is still needed with `--maps`. `dhis2 reports` finds each org
  unit's Location by the identifier system the profile's `[dhis2] base`
  gives, so the maps and the profile have to agree on it. `dhis2 extract`
  needs no `[dhis2]` section (see
  [dhis2.md](dhis2.md#records-and-the-import-package)).
- The profile's URL settings for the built-in mapping are not passed to the
  maps. The example maps read their systems, profiles and extensions as
  parameters of their package, declared in
  [examples/import/icr-dhis2/import.toml](../examples/import/icr-dhis2/import.toml)
  with the values `examples/profiles/icr.toml` gives. `--maps` gives the maps
  the defaults in `import.toml` of the `--maps` folder or of the folder
  above it, and prints which file it read (see
  [import.md](import.md#in-maps)). To change a value, change the default,
  or run the maps with `import run --param`.
- A map writes a resource's elements in the order FHIR defines them. For a
  Measure that puts `subjectCodeableConcept` before `date`, where the
  built-in mapping puts it after `topic`. The content is the same.
- `dhis2 reports --resume` is refused when `--maps` differs from the
  interrupted run's.

## Limits

- A map sees one record. Anything that needs several records, such as
  grouping or a lookup in another file, has to be done before the map runs.
- Maps are read as FHIR R4.
- One transform may create at most 2,000,000 values (the elements it
  writes and the values it works with), and run at most 20,000,000 rules.
  An import package can raise the first with
  [`[maps] max_values`](import.md#maps), for a record that is a large tree.
- A map is slower than the built-in mapping: about 0.25 milliseconds of CPU
  per record with the example maps. `map run` and the `dhis2` commands run
  the map on every core, and the output is the same at any thread count:
  20,000 org units or 20,000 values with a breakdown take 2 to 3 seconds on
  10 cores, and about 10 seconds on one thread. The built-in mapping is still
  faster for `dhis2 reports` runs of many millions of values.
- The engine is [fml-rs](https://github.com/octofhir/fml-rs), first released
  in November 2026.
