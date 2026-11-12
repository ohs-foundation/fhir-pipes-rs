# Design: a general import, with connectors that only extract

Status: proposal, 12 November 2026. Nothing here is built yet except where
it says so. Related notes:
[staged import pipeline](2026-11-11-staged-import-pipeline-and-features-to-add.md),
[ViewDefinitions over logical models](2026-11-11-viewdefinition-over-logical-models.md).

## Summary

Today `fhir-pipes dhis2` reads DHIS2 and turns what it reads into FHIR in one
command. The second half is not specific to DHIS2. This proposal splits it:

- A **connector** reads one source system and writes its records as NDJSON.
  It can be Rust code in fhir-pipes, an OpenFn workflow, a script or a
  database export.
- An **import package** states how those records become FHIR resources:
  logical models, maps, ConceptMaps, and views and SQL where records have to
  be joined or grouped. It is published with an implementation guide.
- **`fhir-pipes import`** runs a package over a folder of NDJSON and writes
  FHIR NDJSON for `fhir-pipes load`. It knows nothing about any source.

DHIS2 and DIGIT become two connectors and two packages.

## The contract between a connector and the import

A connector writes a folder of files:

- one NDJSON file per record type, named after the type (`DIGITTask.ndjson`),
  one record per line, plain or gzip;
- each record an instance of a logical model the package publishes.

That is the whole interface. Whatever produces such a folder can feed the
import. `import check` validates a folder against the package's logical
models before anything is mapped, so a field renamed in an OpenFn job or a
script is reported as an error and not as a value that silently goes missing.

## An import package

A folder with a manifest and the artefacts it names:

```
icr-digit/
  import.toml
  models/        StructureDefinition JSON: one logical model per record type
  maps/          *.map and ConceptMap JSON
  views/         ViewDefinition JSON               (only if records are joined or grouped)
  libraries/     SQLQuery Library sources          (only if records are joined or grouped)
  tests/         example records and the resources expected from them
```

`import.toml` lists the steps in order. A sketch:

```toml
name = "icr-digit"
title = "DIGIT HCM to the Integrated Campaign Registry"
fhir = "4.0.1"

# Records that map on their own: one map, straight to the output.
[[step]]
map = "DIGITFacilityToICR"
from = "DIGITFacility"

[[step]]
map = "DIGITStockToICR"
from = "DIGITStock"

# Records that have to be grouped first.
# 1. Stage each as a FHIR resource. Staged resources are not written to the output.
[[step]]
map = "StageDIGITTask"
from = "DIGITTask"
stage = true

[[step]]
map = "StageDIGITProjectBeneficiary"
from = "DIGITProjectBeneficiary"
stage = true

# 2. Flatten the staged resources and group them.
[[step]]
library = "DigitVisit"          # reads the views it depends on

# 3. Map each row of the result.
[[step]]
map = "DIGITVisitToICR"
from = { library = "DigitVisit" }

# Checks on the result; a failure fails the run.
[[test]]
library = "DigitVisit"
unique = ["visit_key"]
not_null = ["household_id", "worker_id"]
```

A package with only per-record maps needs no manifest beyond its name: each
record file is run through the one map that reads its type, as
`dhis2 --maps` chooses a map today.

## Commands

| Command | What it does |
|---|---|
| `import check --package DIR [--in RAW]` | Type-checks the maps against the models. With `--in`, also checks the records against their models |
| `import run --package DIR --in RAW --out OUT` | Runs the steps. Writes one NDJSON file per resource type in `OUT`, and a run report |
| `import test --package DIR` | Runs the package's tests: example records in, expected resources out |

`map run` and `map check` stay, for running a single map by hand.

Staged resources and intermediate tables are written under a temporary
folder beside `OUT`, mode 0600, and removed when the run ends.

## Connectors

A connector in fhir-pipes does only what needs the source's API:

- `dhis2 extract`: org units, indicators and values. It keeps the request
  planning, resume and `--previous` comparison that `dhis2 reports` has now.
- `digit extract`: paged `_search` calls per entity, with `lastChangedSince`.

Anything else can stand in for a connector. OpenFn is the obvious one where
scheduling, credentials and retries are wanted, or where it already has an
adaptor for the source: an OpenFn workflow extracts, runs `import run`, then
runs `load`. The mapping still lives in the package, not in job code.

## What changes for DHIS2

| Today | Proposed |
|---|---|
| `dhis2 locations`, `measures`, `reports` read and write FHIR | `dhis2 extract` writes records; `import run` writes FHIR. The three commands can stay as shorthands for the two steps |
| Logical models compiled into the binary (`src/dhis2/models`) | In the package |
| Built-in Rust mapping and the `[dhis2]` URL settings in the profile | Removed, once a real DHIS2 run has gone through the maps |
| Preparation in Rust: merged boundaries, the parent check, counts, breakdowns attached to totals | Stays in the connector at first, which emits prepared records as now. Pieces move to SQL in the package only where there is a reason |

The built-in mapping is faster (the maps take about 0.25 ms of CPU per
record). Keeping it as a fast path for very large `reports` runs is the
alternative to removing it.

## What has to be built

1. `import run`, `check` and `test`, and the manifest.
2. A SQLQuery result as the input of a map. Views and Libraries already run
   over local files through `refresh --input`; the import needs the same
   without the warehouse layout.
3. Checking records against a logical model.
4. Package tests and data checks.
5. `digit extract`, and the DIGIT package in the IG, compared with what
   `icr/tools/digit/transform/transform.py` writes for the demo tenant.
6. `dhis2 extract`, and the DHIS2 commands rewritten on top of the import.

Order: 1 to 4 together with the DIGIT package, since DIGIT uses every stage;
then DHIS2; then the removal of the built-in mapping.

## Open decisions

1. **Where packages live.** Proposed: in the implementation guide's
   repository, next to the profiles they target. fhir-pipes keeps small
   examples for its own tests.
2. **The built-in DHIS2 mapping.** Remove it, or keep it as a fast path.
   Proposed: remove, after a real run through the maps.
3. **The manifest format.** TOML, as above and as the Library sources use,
   or a FHIR resource so that the pipeline itself is published in the IG.
   Proposed: TOML to start.
4. **Visit ids under incremental sync** (from the staged pipeline note) is
   still open and matters for DIGIT.
