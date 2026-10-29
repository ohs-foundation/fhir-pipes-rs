# Downloading resources as NDJSON

`export` downloads every resource of each `--type` to `OUT/<TYPE>.ndjson`. It reads a FHIR server by paged search, by Bulk Data `$export` (`--bulk`), or, for HAPI, from its database (`--hapi-db`). `refresh` and the views read the files unchanged. Connection and authentication options are in the [README](../README.md#connecting-to-a-fhir-server) and [servers.md](servers.md). `fhir-pipes export --help` lists every option.

```sh
fhir-pipes export --server https://fhir.example.org/fhir --type Patient --type Observation --out raw/
```

Each file is written beside the target and renamed into place, so a failed run leaves the previous download intact. If the server's count differs from what was downloaded, the run reports `count_mismatch`.

| Option | Default | What it does |
|---|---|---|
| `--type` | | A resource type to download; repeat for several. |
| `--incremental` | off | Fetch only what changed since the last run. |
| `--concurrency` | 4 | How many search requests are in flight at once. |
| `--bulk` | off | Use Bulk Data `$export`. |
| `--bulk-timeout` | 4 hours | How long to wait for a bulk export. |
| `--hapi-db` | off | Read a HAPI server's PostgreSQL database. |
| `--server-kind` | `generic` | `generic`, `hapi`, `medplum` or `aidbox`. |

## Incremental downloads

`OUT/.export-state.json` records the greatest `meta.lastUpdated` downloaded for each type. With `--incremental`, a type that has one is searched with `_lastUpdated=ge<that time>` and merged into its file by id. A `_lastUpdated` search cannot see deleted resources, so after the merge the server's count is compared with the file's, and a type whose counts differ is downloaded in full. `refresh` exports this way unless given `--full`. The saved time is five minutes before the server's clock when the search began, so a resource updated during a run is fetched by the next one.

## Parallel downloads

`--concurrency` is how many requests are in flight at once, for all types together. Up to that many types download side by side. A full download of a type with 10,000 or more resources is split into `_lastUpdated` windows that are paged side by side and joined, so the file has the same resources as a single stream, in a different order. A server that rejects `_sort=_lastUpdated` or whose window counts do not add up is paged as one stream. Incremental downloads stay one stream per type. A `Retry-After` on a 429 or 5xx holds back every request. `--concurrency 1` downloads one page at a time. With `--bulk` it is the number of output files downloaded at once; `--hapi-db` ignores it. For the Aidbox and Medplum window limits see [servers.md](servers.md).

## Bulk Data $export

With `--bulk`, `export` asks the server for a [Bulk Data](https://hl7.org/fhir/uv/bulkdata/) `$export` instead of paging searches. Only system-level export is supported; `Group/[id]/$export` and `Patient/$export` are not.

```sh
fhir-pipes export --bulk --incremental --server https://fhir.example.org/fhir \
  --type Patient --type Observation --out raw/
```

- The kick-off is `GET [base]/$export?_type=…` with `Prefer: respond-async`. With `--incremental`, types that have a saved time are exported with `_since` the earliest of them and merged by id; the others are exported in full.
- The status URL is saved in `.export-state.json` as soon as the server returns it, so a run that stops early polls the same export next time. After `--bulk-timeout` the run cancels the export with a `DELETE` of the status URL and fails.
- A kick-off answered with 429 is retried until `--bulk-timeout` (Aidbox runs one export at a time).
- Output files are downloaded `--concurrency` at a time. The bearer token is sent only when the manifest says `requiresAccessToken`. If a download fails, no file or saved time changes and the next run reads the same export again.
- Resources in the manifest's `deleted` Bundles are removed from the files. Not every server lists deletions (HAPI does not), so the count check still applies after a merge.
- An `OperationOutcome` from the server, or an error or fatal issue in the manifest's `error` files, fails the run. Warnings go in the report.
- After the files are read, the status URL gets a `DELETE` so the server can remove them.

## Reading a HAPI server's database

With `--hapi-db`, `export` reads resources from the PostgreSQL database of a HAPI FHIR JPA server instead of its API, as fhir-data-pipes' `--fhirFetchMode=HAPI_JDBC` does. The connection string comes only from `FHIR_PIPES_HAPI_DATABASE_URL`, never from a flag or a file.

```sh
export FHIR_PIPES_HAPI_DATABASE_URL='postgres://fhir_pipes_ro:...@hapi-db.example.org:5432/hapi?sslmode=verify-full'
fhir-pipes export --hapi-db --incremental --type Patient --type Observation --out raw/
```

- Use `sslmode=verify-full` for a database on another machine. The default, `prefer`, falls back to plain text, and `require` does not check the server's certificate. fhir-pipes warns when a remote database is reached with anything else.
- This reads HAPI's internal tables, which HAPI changes between releases. It was tested with HAPI 8.12 on PostgreSQL 17. fhir-pipes checks for the tables and columns it needs and refuses a database that lacks them. Only PostgreSQL is supported.
- Each resource is the current, not deleted, version of an `hfj_resource` row, read from `hfj_res_ver`. Against HAPI 8.12 the lines match what `export` writes from a search.
- `res_updated` has no time zone. fhir-pipes reads it as UTC, which is what the HAPI Docker images use. On a HAPI server in another time zone, `meta.lastUpdated` is wrong by the offset.
- All types are read in one read-only, repeatable-read transaction, so a run sees one consistent state.
- With `--incremental`, a run reads rows updated after the saved time (kept under `hapi_db` in `.export-state.json`), merges them by id and removes resources deleted since. If the count then differs from the database's, as after an `$expunge`, the type is read in full. Files written by search, bulk or another database are read in full the first time.
- All partitions of a partitioned server go into the same files.

A read-only role for it:

```sql
CREATE ROLE fhir_pipes_ro LOGIN PASSWORD '...';
ALTER ROLE fhir_pipes_ro SET default_transaction_read_only = on;
GRANT CONNECT ON DATABASE hapi TO fhir_pipes_ro;
GRANT USAGE ON SCHEMA public TO fhir_pipes_ro;
GRANT SELECT ON hfj_resource, hfj_res_ver, hfj_res_tag, hfj_tag_def TO fhir_pipes_ro;
GRANT SELECT ON hfj_forced_id TO fhir_pipes_ro;   -- where the table exists
-- Only for resources older HAPI releases stored as large objects; run as a superuser:
GRANT SELECT ON pg_catalog.pg_largeobject TO fhir_pipes_ro;
```

If HAPI's tables are in a schema other than `public`, add `options=-c%20search_path%3D<schema>` to the connection string. `refresh` reads this way with `fetch = "hapi-db"` (see [refresh.md](refresh.md#reading-the-hapi-servers-database)).

## Copying to a second FHIR server

`sync` copies a directory `export` wrote to another FHIR server; see [sync.md](sync.md).
