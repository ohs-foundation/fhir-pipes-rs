# Copying resources to a second FHIR server

`fhir-pipes sync` copies the resources an `export` downloaded to a second FHIR
server, the sink. Run `export` and then `sync`, on a schedule if you like, and
the sink follows the source. It does what the Java pipeline does with
`sinkFhirServerUrl`.

```sh
fhir-pipes export --server $SOURCE --incremental --out raw/ \
  --type Organization --type Location --type Patient --type Encounter --type Observation
fhir-pipes sync --server $SINK --from raw/ \
  --type Organization --type Location --type Patient --type Encounter --type Observation
```

`--from` is the directory `export` wrote, one `<TYPE>.ndjson` per type.
`refresh` has no `sync` step yet; run `sync` after it, on `raw/<name>/`.

## How it works

- Resources go to the sink in transaction Bundles of `PUT <Type>/<id>`, so a
  resource keeps its id. `meta.versionId` and `meta.lastUpdated` are dropped;
  the sink sets its own.
- The second and later runs send only what changed. `sync` records what each
  sink was sent in `.sync-state.json` and `.sync/` in `--from`.
  A resource is sent if it is not in the record or its `meta.lastUpdated`
  differs. `--full` sends everything. To start a sink over, use `--full` or
  remove its entry from `.sync-state.json`.
- Types are sent in the order of `--type`, so list parents first
  (Organization, Location, Patient, Encounter, Observation). Within a type,
  parents come first by `partOf`. A sink that checks references on write (HAPI
  does by default) accepts the copy only if referenced resources are already
  there, or if you turn that check off on the sink.

## Deletions

A resource sent earlier and missing from the download was deleted on the
source, or the download is wrong. `sync` only reports them. With
`--delete`, it sends `DELETE` entries after all the PUTs. If more than
`--max-delete-share` (0.2) of a type's recorded resources are gone, `sync`
deletes none of that type and exits with status 2; `--force-delete` deletes
them anyway.

## Failures and dry run

A failed Bundle is reported, the rest of that type is not sent, and the run
exits with status 1; the next run sends again what failed. `--dry-run` prints
the plan without sending.

## The sink server

- HAPI: ids that are only digits need `hapi.fhir.client_id_strategy=ANY`.
- Cloud Healthcare API: set `enableUpdateCreate` on the store; use the store
  path as `--server` and `--google-auth`.

Authentication options are the sink's (see
[servers.md](servers.md#connecting-to-a-server)).

## Options

`--help` lists them all.

| Option | Default | Meaning |
|---|---|---|
| `--server` | | The sink's base URL, or a Cloud Healthcare API store path |
| `--from` | | The directory `export` wrote |
| `--type` | every `<TYPE>.ndjson` | Types to copy, in this order; repeat for several |
| `--full` | | Send every resource, not only what changed |
| `--delete` | | Delete from the sink what is gone from the download |
| `--max-delete-share` | 0.2 | Largest share of a type deleted in one run |
| `--batch-size` | 100 | Resources per transaction Bundle |
