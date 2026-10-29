# Value sets and concept maps as tables

`fhir-pipes terminology` writes a value set or a concept map from FHIR
packages as a table, so a query can join a view's rows to it. The columns are
those defined by SQL on FHIR's
[Terminology in SQL](https://build.fhir.org/ig/HL7/sql-on-fhir/en/terminology-in-sql.html)
page. No terminology server is called: the content comes from the packages
alone.

```sh
fhir-pipes terminology valueset http://hl7.org/fhir/ValueSet/administrative-gender \
  --package hl7.fhir.r4.core#4.0.1
```

Use `terminology conceptmap` the same way for a concept map.

| Option | Meaning |
|---|---|
| first argument | The canonical URL. Add `\|version` to pick one; without it, more than one version in the packages is an error |
| `--package` | A package to read. Give it once per package the content needs. A `package.tgz` (path or https URL), a directory of JSON resources, or `name#version` from `~/.fhir/packages` |
| `--output`, `--out` | `csv` (default), `ndjson`, `json` or `parquet`; the file to write instead of stdout. As for `views run` |

## Value set table

One row per member: `system`, `version`, `code`, `display`, `inactive`
(`true` for an inactive code; null where nothing says).

Members come from the value set's `expansion` when the package has one.
Otherwise fhir-pipes expands its `compose` from listed codes, complete code
systems the packages hold, the `is-a`, `descendent-of` and `is-not-a` filters,
and other value sets it includes or excludes.

Some value sets need more than the packages hold, such as SNOMED CT or LOINC.
The command then fails and says what is missing. It does not write a table
with members missing.

## Concept map table

One row per mapping target: `source_system`, `source_version`, `source_code`,
`source_display`, the same four `target_*` columns, and `relationship`, an R5
ConceptMapRelationship code. An R4 `equivalence` is written as the R5
`relationship` it became. `group.unmapped` gives no rows. Maps that cannot be
written as rows (a group without a source, a mapping to or from a value set, a
target with `dependsOn` or `product`) fail.

## Joining to a view

```sh
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
