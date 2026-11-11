# Staged import pipeline, and features to add to it

Notes from 11 November 2026. Related:
[ViewDefinitions over logical models](2026-11-11-viewdefinition-over-logical-models.md),
which proposes a spec change. The approach here needs no spec change and is
the one we prefer to build first.

## The approach

Most source records map on their own: a logical model describes the record, an
FML map turns it into the target resource, and ConceptMaps translate the
codes. That covers DHIS2 org units and indicators, and DIGIT facilities,
boundaries, stock and referrals.

A few mappings depend on more than one source record, such as grouping
per-person DIGIT tasks into one household visit Task. For those:

1. **Extract** raw records from the source into NDJSON. Code, per source.
2. **Stage** each record one-to-one as a FHIR resource with an FML map. No
   joins. The staged resources carry a staging tag.
3. **Flatten** the staged resources with standard ViewDefinitions.
4. **Join and group** with SQLQuery Libraries over those views.
5. **Map** each result row to the target resources with an FML map. The row is
   described by a logical model.
6. **Validate and load.**

The staged resources are NDJSON files outside the FHIR server. They are not
needed after the load and are deleted. Keep the raw extract until the next
run, for comparing runs and for re-running after a rule change. Staging files
hold personal data: write them mode 0600 and remove them when the run ends.

Everything in steps 2 to 5 is a standard FHIR artefact and can be published in
the implementation guide.

## What was tested

A small synthetic set: 4 tasks, 3 project beneficiaries, 3 household members.
As in DIGIT, a task names only its project beneficiary, so finding the person
and the household takes two joins.

- Staging: 3 FML maps and 1 ConceptMap, run with `fml` 0.2.7, gave 4 Tasks and
  6 Basic resources.
- Flatten, join and group: `fhir-pipes refresh --input` ran 3 ViewDefinitions
  and 1 SQLQuery Library unchanged and gave 3 visit rows.
- Final map: 3 visit Tasks and 4 MedicationAdministrations, with a morning
  visit and an afternoon revisit to the same household kept apart, and a visit
  to an absent household marked `failed`.

Not tested: real DIGIT data, volume, validation against the ICR profiles, the
load, or the rest of the mapping (products, registrations, site sessions,
reason codes).

Artefacts for this one mapping: 4 logical models, 3 staging maps, 1
ConceptMap, 3 ViewDefinitions, 1 SQLQuery Library, 1 final map.

## Work needed in fhir-pipes

- **A map command.** Run an FML map over every line of an NDJSON file and
  write NDJSON. In the test this was a loop over the `fml` binary, one record
  per call. A bulk run should use the `fml-engine` library.
- **Library results as map input.** Read a SQLQuery result table as the input
  of the final map. In the test the Parquet rows were exported to JSON with
  DuckDB and the columns renamed by hand.
- **The FHIRPath upgrade.** `fml-engine` needs `octofhir-fhirpath` 0.5.x and
  Rust 1.98 or later. In a scratch copy the upgrade from 0.4.57 needed one
  match arm in `crates/sof/src/eval/rewrite.rs` and one in
  `crates/sof-lint/src/shareable.rs`. The FHIRPath suites then had 30 new
  failures to triage and 345 listed failures that now pass.
- **A lighter run mode.** `refresh` writes its warehouse layout, which is more
  than an import needs.

## Features to add, using dbt as the comparison

The pipeline has the same shape as dbt: staged inputs, declared dependencies,
SQL run in dependency order. dbt is useful as a checklist of what a mature
transformation tool provides. In priority order:

### 1. Data tests on results

Declare checks on a view or Library result, run them after it, and fail the
run on a violation. Today `views lint`, `views validate` and `views test`
check definitions, not the data produced. DIGIT's `check.py` checks the data
by hand.

| Test | Example from the visit mapping |
|---|---|
| `not_null` | Every visit has a household and a worker |
| `unique` | `visit_key` is unique |
| `relationships` | Every task's beneficiary exists in the beneficiary table |
| `accepted_values` | `digit_status` is one of the statuses the ConceptMap covers |
| Custom SQL that must return no rows | No task appears in two visits; no task is dropped by the joins |

The last two matter most. The test Library used inner joins, so a task whose
beneficiary was missing would have disappeared with no message.

### 2. Unit tests with fixed inputs and expected outputs

A test case is a few input records and the expected result. It applies to
Libraries and to FML maps. Cases can be published in the IG as examples, so a
second implementation can run them. First case: the morning visit and
afternoon revisit.

### 3. Library parameters

A Library can declare `[[parameter]]` but execution ignores it. The 2-hour
visit window and tenant-specific values should be parameters.

### 4. A check that the stages agree

Check before the run that a Library's result columns match the logical model
the final map reads, in names and types.

### 5. Counts and dropped records per stage

`manifest.json` has row counts per table. Add counts in and out for each
stage, with the reason for each difference, as `transform.py` writes in
`report.json`.

### 6. Lineage documentation

The dependency graph is already in the `depends-on` entries. Render it as an
IG page, from source record to target resource.

### Lower priority

- **Incremental processing.** Full recomputation avoids the late-arriving-task
  problem below. Add it only when a full run is too slow.
- **Snapshots, macros, templating.** Not needed. Templating would make the
  published SQL harder to read.

## Open issues

- **Visit ids under incremental sync.** The visit id comes from the first task
  in the group. A task that syncs late and sorts earlier changes the id of an
  existing visit and leaves a duplicate. This affects `transform.py` today. It
  needs a rule, such as recomputing a household's visits for a round whenever
  one of its tasks changes, and removing the visits that no longer exist.
- **Records with no natural FHIR resource.** A project beneficiary or a
  household member is only a link. In the test these were staged as Basic with
  an extension, which works but gives long view paths.
- **Tests as published artefacts.** Whether SQL on FHIR has or plans a standard
  way to attach tests to a Library. Until then they are `fhir-pipes`
  conventions in the Library's `.toml`.
- **SQL dialect.** The test Library uses DuckDB functions (`list`, `epoch`,
  struct literals).
- **Maturity of the FML engine.** `fml-rs` was first published on 1 November
  2026.

## Next step

Run the pipeline on a slice of the real DIGIT demo extract and compare the
visit Tasks with what `transform.py` produces for the same records.
