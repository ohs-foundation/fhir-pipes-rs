# Value sets and concept maps as tables

`fhir-pipes terminology` writes a value set or a concept map from FHIR
packages as a table. A query can then join a view's rows to it like any
other table. The tables have the columns SQL on FHIR's
[Terminology in SQL](https://build.fhir.org/ig/HL7/sql-on-fhir/en/terminology-in-sql.html)
page defines, so a query written against that page runs unchanged once each
table is loaded under the name the query uses. No terminology server is
called: the content comes from the packages alone.

```sh
fhir-pipes terminology valueset http://hl7.org/fhir/ValueSet/administrative-gender \
  --package hl7.fhir.r4.core#4.0.1
```

```
system,version,code,display,inactive
http://hl7.org/fhir/administrative-gender,4.0.1,female,Female,
http://hl7.org/fhir/administrative-gender,4.0.1,male,Male,
http://hl7.org/fhir/administrative-gender,4.0.1,other,Other,
http://hl7.org/fhir/administrative-gender,4.0.1,unknown,Unknown,
```

## Options

- The first argument is the canonical URL. Add `|version` to pick a version
  when the packages hold more than one; without it, more than one version is
  an error.
- `--package` names a package to read. Give it once per package the content
  needs: an IG's value set may include code systems from
  `hl7.fhir.r4.core` or `hl7.terminology`. A package can be:
  - a `package.tgz` file, as a path or an https URL;
  - an unpacked package, or any directory of JSON resources such as SUSHI's
    `fsh-generated/resources`;
  - `name#version` for a package in the local FHIR package cache
    (`~/.fhir/packages`), where the IG publisher, SUSHI and the validator
    keep them.
- `--output` is `csv` (the default), `ndjson`, `json` or `parquet`, and
  `--out` names the file to write instead of stdout. These work as they do
  for `views run`.

## The value set table

| Column | Type | Content |
| --- | --- | --- |
| `system` | string | The code system |
| `version` | string | The code system version used; null where none is recorded |
| `code` | string | The member code |
| `display` | string | Its display text |
| `inactive` | boolean | `true` for a code inactive in that version; null where nothing says |

There is one row per member, and no two rows have the same `system`,
`version` and `code`. To leave out inactive codes, filter on
`inactive IS NULL OR inactive = false`; an active code usually has null.

The members come from the value set's `expansion` when the package has one.
Otherwise fhir-pipes expands its `compose`, from:

- codes the value set lists;
- every code of a code system the packages hold in full
  (`content = complete`), leaving out codes marked `notSelectable`;
- `concept` filters `is-a`, `descendent-of` and `is-not-a` over such a code
  system's hierarchy, given by nesting or by `parent` properties;
- other value sets it includes or excludes.

The `version` column is the version the `include` names. If the include names
none, it is the version of the code system read from the packages.

Some value sets need more than the packages hold: SNOMED CT or LOINC, a code
system a package lists as `not-present`, or a filter other than those above.
For these the command fails and says what is missing. It does not write a
table with members missing. Such a value set needs a terminology server to
expand it, or a package that ships its expansion. From `hl7.fhir.r4.core`
alone, 1,217 of its 1,316 value sets can be written.

## The concept map table

| Column | Content |
| --- | --- |
| `source_system`, `source_version`, `source_code`, `source_display` | The code mapped from |
| `target_system`, `target_version`, `target_code`, `target_display` | The code mapped to |
| `relationship` | How they relate, as an R5 ConceptMapRelationship code |

There is one row per mapping target. An element that maps to nothing
(`noMap` in R5, a target with equivalence `unmatched` in R4) gives a row with
no target code or relationship. An R4 map's `equivalence` is written as the R5
`relationship` it became:

| R4 equivalence | relationship |
| --- | --- |
| `equivalent`, `equal` | `equivalent` |
| `wider`, `subsumes` | `source-is-narrower-than-target` |
| `narrower`, `specializes` | `source-is-broader-than-target` |
| `relatedto`, `inexact` | `related-to` |
| `disjoint` | `not-related-to` |

`group.unmapped` gives no rows. To fall back to the source code, write it in
SQL, as `COALESCE(map.target_code, conditions.code)`. Some maps cannot be
written as rows, and the command fails for them: a group without a source, a
mapping to or from a value set, or a target with `dependsOn` or `product`.

## Using the tables

Write the views and the value set, then query them together. In DuckDB:

```sh
fhir-pipes views run views/condition.json --input raw/ --output parquet --out condition.parquet
fhir-pipes terminology valueset https://example.org/ValueSet/cvd \
  --package ig/package.tgz --package hl7.fhir.r4.core#4.0.1 \
  --output parquet --out cvd_codes.parquet
```

```sql
SELECT c.patient_id, c.code
FROM 'condition.parquet' AS c
WHERE EXISTS (
  SELECT 1 FROM 'cvd_codes.parquet' AS v
  WHERE v.system = c.system AND v.code = c.code
);
```

A semi-join (`EXISTS`) tests membership without multiplying rows when a code
is in the value set under more than one code system version.
