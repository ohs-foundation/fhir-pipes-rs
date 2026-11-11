# Proposal: ViewDefinitions over logical models, for publishing source-system mappings in an IG

Status: draft for discussion, 11 November 2026.

## Summary

We want an implementation guide to carry the complete, executable mapping from
a non-FHIR source system into the IG's profiles. FHIR Mapping Language (FML)
maps and ConceptMaps cover most of it. They do not cover the part of a mapping
that depends on more than one source record, such as grouping several records
into one resource.

SQL on FHIR already has the right tools for that part: a ViewDefinition to
describe a table, and a SQLQuery Library to hold a query over tables. The
obstacle is that `ViewDefinition.resource` has a required binding to the FHIR
resource types, so a view cannot be defined over source records described by a
logical model.

We propose allowing a ViewDefinition to be based on a logical model. This note
describes the use case, the proposal, what we have tested and the questions we
would like the community's view on.

## The use case

The Integrated Campaign Registry (ICR) is a FHIR R4 implementation guide for
public health campaign data: campaigns, target populations, household visits,
doses given and stock. Data reaches it from systems that are not FHIR. Two we
import today:

- **DHIS2**, an aggregate reporting system. Org units become Locations,
  indicators become Measures, and indicator values become MeasureReports.
- **DIGIT HCM**, a campaign delivery platform. Its records for facilities,
  individuals, households, tasks, stock and referrals become thirteen FHIR
  resource types.

Each importer is custom code today: one in Rust, one in Python. The mapping
itself is documented in prose. Someone who wants to know how a DIGIT task
becomes an ICR Task has to read the prose or the code, and a second
implementation would have to re-derive the rules from them.

We would like the IG to state the mapping in a form that is published,
versioned with the profiles it targets, checked when the IG builds, and
executable by any conforming tool.

## What FML and ConceptMap already cover

For a source record that maps to one or two resources on its own, the standard
approach works:

1. A logical model (StructureDefinition) describes the source record.
2. An FML map (StructureMap) turns one instance of it into FHIR resources.
3. ConceptMaps hold the code translations, called from the map with
   `translate()`.

We tested this for the DHIS2 org unit. A 58-line FML map, a logical model with
14 elements and one ConceptMap produced Locations and Organizations identical
to the output of the existing Rust code, for four org units covering the main
cases. A type checker validated the map against both the logical model and the
target resources.

## The gap: mappings that depend on more than one record

An FML map is given one input and produces output from that input alone. Some
of what the ICR needs cannot be computed from one source record.

The clearest case is the household visit. DIGIT writes one task per person per
delivery. The ICR stores one Task per household visit, with:

- an `output` reference to each person's delivery event,
- a count of people treated,
- a status reduced from every person's outcome.

DIGIT has no visit identifier. The importer groups tasks that share a campaign
round, household and worker, and splits a group where more than two hours pass
between consecutive tasks, so a morning attempt and an afternoon revisit are
two visits.

Other cases of the same kind in the DIGIT import:

- Whether an individual becomes a Patient or a Practitioner depends on which
  other records refer to them.
- A task's campaign round is found by walking the project tree to its root.

FML can compute the visit's count, status and outputs if its input is the
whole visit, with the per-person tasks nested inside it. We tested that with a
35-line map. What FML cannot do is assemble that input: decide which source
records belong to the same visit.

That assembling step is joins, grouping and window functions over sets of
records, which is what SQL is for.

## What SQL on FHIR already provides

- A **ViewDefinition** describes a table as FHIRPath expressions over one type
  of record.
- A **SQLQuery Library** is a FHIR Library holding a SQL statement, with the
  views it reads declared as `depends-on` related artifacts.

Both are FHIR resources and both can be published in an IG. If the grouping
step were a SQLQuery Library over views of the source records, every part of
the mapping would be a published artefact.

## The obstacle

`ViewDefinition.resource` is a `code` with a required binding to
`http://hl7.org/fhir/ValueSet/resource-types`. A view can therefore only be
based on a FHIR resource type. Source records described by a logical model are
outside it.

## The proposal

Allow a ViewDefinition to be based on a logical model.

A ViewDefinition evaluates FHIRPath over a typed structure. A logical model is
a typed structure defined by a StructureDefinition, and FHIRPath evaluates
over it in the same way. FML already treats logical models as first-class
sources and targets. Nothing in the view's `select`, `forEach`, `where` or
`column` semantics depends on the input being a resource.

Possible forms, for discussion:

1. **Relax the binding.** Make the `resource` binding extensible, and allow a
   logical model's type name or canonical URL as the value.
2. **Add an element or extension.** Keep `resource` as it is, and add a way to
   name a logical model's canonical URL as the basis of the view, used in place
   of `resource`.
3. **A separate profile.** Define a profile of ViewDefinition for logical
   model sources, so that the existing shareable and tabular profiles are
   unaffected.

Points the specification would need to address:

- **Identity.** Views over resources rely on `id` and on `getResourceKey()`
  and `getReferenceKey()`. A logical model instance may have no `id`. The view
  would use ordinary key columns, and the two functions would be undefined or
  disallowed for logical model views.
- **Type information.** Engines that use a FHIR model for type checking and
  for `ofType()` would need the logical model's StructureDefinition, loaded
  from a package in the same way as profiles.
- **Instance format.** The JSON form of a logical model instance, including
  how the instance declares its type, should follow whatever FHIR core and the
  mapping language settle on.
- **Scope.** This proposal concerns views used as inputs to a transformation.
  It does not ask FHIR servers to store or serve logical model instances.

## The resulting pipeline

| Step | Artefact | Published in the IG |
|---|---|---|
| Extract records from the source API | Code, per source | No |
| Describe each source record | Logical model | Yes |
| Present source records as tables | ViewDefinition over the logical model | Yes (this proposal) |
| Join, group and nest records | SQLQuery Library | Yes |
| Describe the grouped record | Logical model | Yes |
| Produce FHIR resources | FML map | Yes |
| Translate codes | ConceptMap | Yes |
| Validate and load | Shared tooling | No |

Only the extraction is custom code. The IG can also carry example source
records and the expected output, so that any implementation can be tested
against it.

### When the view and SQL steps are used

They are the exception. Most source records map on their own, and for those
the pipeline is the logical model, the FML map and ConceptMaps, with no view
and no SQL. The view and SQLQuery steps are added only where the output
depends on more than one source record:

- several source records become one resource (per-person tasks to one visit
  Task),
- a resource's content depends on which other records refer to it (an
  individual as Patient or Practitioner),
- a value has to be found by following links between records (a task's
  campaign round).

In the DIGIT import, facilities, boundaries, stock and referrals need neither
step. In the DHIS2 import, org units and indicators need neither; the
geometry handling for Locations stays in code.

## Worked example: DIGIT tasks to an ICR visit Task

The examples are shortened. Element names are illustrative.

Logical model of the source record:

```fsh
Logical: DIGITTask
Id: DIGITTask
Title: "DIGIT task"
* clientReferenceId 1..1 string "Client reference id"
* roundId 1..1 string "Campaign round"
* householdId 1..1 string "Household"
* workerId 1..1 string "Worker who recorded the task"
* personId 1..1 string "Beneficiary"
* status 1..1 code "DIGIT task status"
* time 1..1 dateTime "Client time of the task"
```

ViewDefinition over it. This is the part the specification does not allow
today:

```json
{
  "resourceType": "ViewDefinition",
  "url": "https://icr.healthcampaigns.org/ViewDefinition/DigitTask",
  "name": "DigitTask",
  "status": "draft",
  "resource": "DIGITTask",
  "select": [{
    "column": [
      {"name": "crid", "path": "clientReferenceId", "type": "string"},
      {"name": "round", "path": "roundId", "type": "string"},
      {"name": "hh", "path": "householdId", "type": "string"},
      {"name": "worker", "path": "workerId", "type": "string"},
      {"name": "person", "path": "personId", "type": "string"},
      {"name": "status", "path": "status", "type": "code"},
      {"name": "t", "path": "time", "type": "dateTime"}
    ]
  }]
}
```

SQLQuery Library that groups tasks into visits. It depends on the view above
under the label `digit_task`:

```sql
WITH gaps AS (
  SELECT *, epoch(t) - epoch(lag(t) OVER w) AS gap
  FROM digit_task
  WINDOW w AS (PARTITION BY round, hh, worker ORDER BY t, crid)
), numbered AS (
  SELECT *, sum(CASE WHEN gap IS NULL OR gap > 7200 THEN 1 ELSE 0 END)
            OVER (PARTITION BY round, hh, worker ORDER BY t, crid) AS n
  FROM gaps
)
SELECT first(crid ORDER BY t, crid) AS visitKey,
       round AS roundId, hh AS householdId, worker AS workerId,
       list({crid: crid, personId: person, status: status, time: t}
            ORDER BY t, crid) AS task
FROM numbered
GROUP BY round, hh, worker, n
```

Each result row is an instance of a second logical model, `DIGITVisit`, with
the tasks nested. An FML map turns it into the visit Task and the delivery
events:

```
group VisitToTask(source src : DIGITVisit, target tk : Task) {
  src -> tk.id = evaluate(src, 'dgt-visit-' + visitKey) "id";
  src where task.where(status = 'ADMINISTRATION_SUCCESS' or status = 'BENEFICIARY_REFUSED').exists()
      -> tk.status = 'completed' "reached";
  src -> tk.for as r, r.reference = evaluate(src, 'Group/dgt-' + householdId) "for";
  src -> tk.output as o, o.type as ty, ty.text = 'treated-count',
      o.value = evaluate(src, task.where(status = 'ADMINISTRATION_SUCCESS').personId.distinct().count()) "treated";
  src.task as t -> tk.output as o, o.type as ty, ty.text = 'delivery-event',
      o.value = create('Reference') as r,
      r.reference = evaluate(t, 'MedicationAdministration/dgt-' + clientReferenceId) "outputs";
}
```

## What we have tested

All tests were small and used synthetic or stub data.

- **Per-record FML with a logical model source.** DHIS2 org unit to Location
  and Organization, four records, output identical to the existing importer.
  Engine: `fml` 0.2.7 from the `fml-rs` project.
- **Grouping in SQL.** The query above, reading four task records directly
  from NDJSON in DuckDB, produced three visits, with a morning visit and an
  afternoon revisit to the same household kept apart.
- **FML over the grouped record.** A map over `DIGITVisit` produced one Task
  with the reduced status, the treated count, the execution period and an
  output reference per event, plus one MedicationAdministration per task.
- **A ViewDefinition over a logical model.** The view engine in `fhir-pipes`
  ran a ViewDefinition with `resource` set to a logical model type over the
  raw task records and returned the expected table.

Not yet tested:

- The steps joined end to end, with the SQL run as a SQLQuery Library reading
  the view.
- The full DIGIT mapping, which also covers products and co-packaged drugs,
  registrations, site sessions and reason codes.
- Real data volumes. The DIGIT demo tenant is about 3 GB of raw records.
- Any other SQL on FHIR or FML engine.

## Alternatives considered

- **Custom code with the mapping described in prose.** This is the current
  state. The mapping cannot be checked or executed from the IG.
- **A workflow tool such as OpenFn.** Good for scheduling, credentials and
  retries, and it can run the steps above. On its own, the mapping lives in
  job code inside one deployment and is not part of the IG.
- **Map every source record to a FHIR resource first, then use standard
  ViewDefinitions.** This stays within the current specification. Some source
  records produce no resource in the target IG on their own (a visit to a
  closed household gives no delivery event) yet the grouped resource depends
  on them. That forces an intermediate set of resources that exists only to
  feed the grouping and must not be loaded, and a second mapping pass.
- **The same SQL as a Library that reads the raw files directly.** This works
  without any change to the specification. The inputs are then not declared as
  views, and the query is tied to a file layout instead of a described
  structure.
- **CQL.** It can express grouping and aggregation and is publishable as a
  Library. It needs a CQL engine and, like SQL on FHIR, it expects FHIR data
  as input.

## Questions for the community

1. Is a ViewDefinition based on a logical model acceptable in principle, and
   which of the three forms above is preferred?
2. Has this been discussed before in the SQL on FHIR group, and is there
   existing guidance or prior work to build on?
3. Is there an established pattern for publishing source-system mappings that
   need grouping across records, in WHO SMART Guidelines or elsewhere, that we
   should follow instead?
4. Should the result of a SQLQuery Library be describable by a logical model,
   so that a map can declare it as its source and a type checker can verify
   the two agree?
5. SQL dialects differ. The example uses DuckDB functions (`list`, `epoch`,
   struct literals). Is per-dialect content in a Library sufficient for a
   published mapping, or is a portable subset needed?

## References

- SQL on FHIR, ViewDefinition: https://build.fhir.org/ig/HL7/sql-on-fhir/StructureDefinition-ViewDefinition.html
- SQL on FHIR, SQLQuery: https://build.fhir.org/ig/HL7/sql-on-fhir/en/StructureDefinition-SQLQuery.html
- FHIR Mapping Language: https://build.fhir.org/ig/HL7/mapping-language-ig/
- WHO SMART Guidelines, StructureMaps: https://smart.who.int/ig-starter-kit/v1.0.0/l3_structuremaps.html
- fml-rs: https://github.com/octofhir/fml-rs
- fhir-pipes: https://github.com/ohs-foundation/fhir-pipes-rs
