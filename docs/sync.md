# Copying resources to a second FHIR server

`fhir-pipes sync` copies the resources an `export` downloaded to a second FHIR
server, called the sink. Run `export` and then `sync`, on a schedule if you
like, and the sink follows the source. This is what the Java pipeline does
with `sinkFhirServerUrl` (`--fhirSinkPath`), for example to keep a Shared
Health Record up to date from a facility's server.

```sh
fhir-pipes export --server $SOURCE --incremental --out raw/ \
  --type Organization --type Location --type Patient --type Encounter --type Observation
fhir-pipes sync --server $SINK --from raw/ \
  --type Organization --type Location --type Patient --type Encounter --type Observation
```

`--from` is the directory `export` wrote: one `<TYPE>.ndjson` per type.
Without `--type`, every `<TYPE>.ndjson` in it is copied, in name order.

## What is sent

Resources go to the sink in transaction Bundles of `PUT <Type>/<id>`, so a
resource keeps its id. Unlike `load`, the entries have no `ifMatch`, and each
resource's `meta.versionId` and `meta.lastUpdated` are dropped: they belong
to the source server, and the sink sets its own. The rest of `meta` (tags,
profiles, security labels, `source`) is sent as it is.

`--batch-size` (100) is the number of resources in a Bundle, and
`--concurrency` (1) the number of Bundles sent at once. Each Bundle is a
transaction, so the sink writes all of it or none of it.

## Only what changed

The second and later runs to a sink send only what changed. `sync` keeps a
record of what each sink was sent in the `--from` directory:

- `.sync-state.json` names, for each sink URL, a directory under `.sync/`,
  with a summary per type: how many resources the sink has from these runs,
  the newest `meta.lastUpdated` among them, and when the type was last sent.
- `.sync/<n>/<TYPE>.tsv` is one line per resource the sink has from these
  runs: its id and its `meta.lastUpdated`.

A resource is sent when its id is not in the record, when its
`meta.lastUpdated` differs from the recorded one, or when it has no
`meta.lastUpdated`. `--full` sends every resource. A resource is recorded
only after the Bundle holding it is committed, so after a failure the next
run sends again whatever was not written.

The record is per resource rather than one time per type: it is also what
`--delete` needs, and it stays right when a resource shows up in a later
download with a `meta.lastUpdated` older than resources already sent (a
long transaction on the source can do that). Its size grows with the number
of resources, about 70 bytes each on disk with UUID ids; a type's record is held in memory
while that type is sent.

Each sink has its own record, so one download can be copied to several
servers. To start a sink over, remove its entry from `.sync-state.json` (and
its `.sync/<n>` directory), or run with `--full`.

## Order

Every file is read and checked before anything is sent: each line must be a
resource of its file's type with a valid FHIR id, and no id may appear twice.
A bad file stops the run before any request.

Types are sent in the order given with `--type`, so give parents' types
first, for example Organization, Location, Patient, Encounter, Observation.
Within a type, resources are sent parents first by `partOf`, as `load` does:
the roots of a Location or Organization hierarchy, then their children, and
so on. Other references inside a type are not ordered.

A sink that checks references on write (HAPI does by default) accepts the
copy only when every referenced resource is already there. Either send the
types in that order, or turn the check off on the sink, as the fhir-data-pipes
test sink does (`hapi.fhir.enforce_referential_integrity_on_write=false`).

## Deletions

A resource the sink was sent earlier and that is no longer in the download
was deleted on the source, or the download is wrong. `export` drops deleted
resources from its files when a type's count differs from the server's (the
type is then downloaded in full), and `export --bulk` also drops the
resources its manifest lists as deleted.

By default `sync` only reports them:

```
Observation: 0 sent, 115 unchanged, 0 deleted, 0 failed; 5 gone from the download, not deleted without --delete
```

With `--delete`, they are sent as `DELETE <Type>/<id>` entries in transaction
Bundles, after every PUT of the run and with the last type first, so that a
resource is deleted before the resources it references. A deleted resource
is removed from the record.

An empty or truncated download would otherwise delete most of the sink. If
more than `--max-delete-share` (0.2, a fifth) of a type's recorded resources
are gone, `sync` deletes none of that type, says so, and exits with status 2:

```
Patient: 8 of 8 synced resources (100%) are gone from raw/Patient.ndjson; that is more than --max-delete-share 0.2, so none were deleted. Check the download, or pass --force-delete
```

`--force-delete` deletes them anyway.

A sink that checks references on delete (HAPI does by default) refuses to
delete a resource that others still reference. Resources gone in the same run
are deleted last type first, but a parent and its child in the same type can
land in different Bundles; set `--batch-size` high enough to hold them, or
turn the check off (`hapi.fhir.enforce_referential_integrity_on_delete=false`).

## Failures

A Bundle that fails is reported with the sink's response. The rest of that
type is not sent; the other types are. At the end, each type gets one line:

```
Patient: 2 sent, 0 unchanged, 0 deleted, 2 failed
```

and the run exits with status 1. The next run sends again what failed. 429,
5xx and connection errors are retried first, up to `--retries` times
(3), as for every command; `--timeout` (300 seconds) is the deadline for
each request.

## Dry run

`--dry-run` reads and checks the files, runs the preflight against the sink,
and prints what would be sent and deleted, without sending anything or
changing the record:

```
Dry run: nothing sent to http://sink:8080/fhir
Patient: 3 to send, 37 unchanged
Observation: 0 to send, 115 unchanged, 5 to delete: obs-0-0, obs-0-1, obs-1-0, obs-1-1, obs-1-2
```

## The sink server

Before sending, `sync` reads the sink's `/metadata`. That checks the URL and
credentials, and that the sink lets `PUT` create a resource (update as
create): a type the CapabilityStatement marks `updateCreate: false` stops the
run. HAPI supports it without stating it, which gives a warning.

- HAPI: ids that are only digits need `hapi.fhir.client_id_strategy=ANY`.
- Cloud Healthcare API: set `enableUpdateCreate` on the store. Give the store
  path as `--server` (`projects/P/locations/L/datasets/D/fhirStores/S`) and
  use `--google-auth`. The account needs `roles/healthcare.fhirResourceEditor`.
  To accept references to resources not yet copied, create the store with
  `disableReferentialIntegrity` set to `true`; it can only be set when the
  store is created.

## Authentication

`sync` talks only to the sink, so the usual options are the sink's:
`--token`, `--client-id`/`--client-secret`/`--token-url`,
`--username`/`--password` and `--google-auth`, or their `FHIR_PIPES_*`
environment variables (see [Connecting to a FHIR server](../README.md#connecting-to-a-fhir-server)).
The source's credentials are only needed by `export`. When the two need
different credentials, set them per command, for example with the
environment variable on each command line.

## Options

| Option | Default | Meaning |
|---|---|---|
| `--server` | | The sink's base URL, or a Cloud Healthcare API store path |
| `--from` | | The directory `export` wrote |
| `--type` | every `<TYPE>.ndjson` | The types to copy, in this order; repeat for several |
| `--full` | | Send every resource, not only what changed |
| `--delete` | | Delete from the sink what is gone from the download |
| `--max-delete-share` | 0.2 | With `--delete`, the largest share of a type deleted in one run |
| `--force-delete` | | With `--delete`, delete even above `--max-delete-share` |
| `--dry-run` | | Print the plan; send nothing |
| `--batch-size` | 100 | Resources per transaction Bundle |
| `--concurrency` | 1 | Bundles sent at once |
| `--retries` | 3 | Attempts per request |
| `--timeout` | 300 | Seconds allowed for each request |

`refresh` has no `sync` step yet; run `sync` after it, on its `raw/<name>/`
directory.
