# Connectors

A connector is a program that reads one source system, such as DHIS2 or
DIGIT, and writes what it reads as records for `fhir-pipes import run`. It
does only what needs the source's API: requests, paging, credentials,
retries, and resuming an interrupted run. How the records become FHIR
resources is in an import package, not in the connector (see
[import.md](import.md)).

A connector is a separate program. fhir-pipes does not load connectors as
libraries or plugins.

## What a connector writes

A connector writes a folder:

- one NDJSON file per record type, named after the type
  (`DHIS2OrgUnit.ndjson`), or `<Type>.ndjson.gz` for gzip;
- one record per line, each a JSON object that is an instance of a logical
  model in the import package. A record's type is the last path segment of
  its model's URL.

That folder is the whole interface. `import run` reads it:

```sh
my-connector --out raw/
fhir-pipes import check --package PACKAGE --in raw/
fhir-pipes import run   --package PACKAGE --in raw/ --out fhir/
fhir-pipes load --server URL --in fhir/Location.ndjson
```

`import check --in` checks the records against the package's logical
models, so a field a connector renamed is reported before anything is
mapped. `import run` reads only `*.ndjson` and `*.ndjson.gz` files and
ignores the rest, so a connector can keep its own state files in the folder
under other names. Write each file under a temporary name and rename it
when it is complete, so that an interrupted run does not leave a partial
file that `import run` would read.

## Any program can be a connector

A connector can be written in any language. Examples:

- A Python script that pages through an API and writes one file per type.
  The DIGIT connector is one: it stays in Python, in the implementation
  guide's repository.
- An OpenFn workflow, where scheduling, credentials and retries are wanted,
  or where OpenFn already has an adaptor for the source. The workflow
  extracts, runs `fhir-pipes import run`, then runs `fhir-pipes load`.
- A database export that writes NDJSON.
- `fhir-pipes dhis2 extract`, below.

## Running a connector as a fhir-pipes command

`fhir-pipes NAME ARGS…`, where NAME is not a fhir-pipes command, runs the
program `fhir-pipes-NAME` with ARGS, as git and cargo run theirs. So a
program installed as `fhir-pipes-digit` can be run as `fhir-pipes digit`.

- fhir-pipes looks for `fhir-pipes-NAME` first in the folder of the
  running `fhir-pipes`, then in each folder of the `PATH`. On Windows it
  tries the extensions in `PATHEXT` (`.exe`, `.cmd` and so on). On Unix the
  file must be executable.
- The program gets the arguments after NAME unchanged, and the same stdin,
  stdout and stderr. fhir-pipes exits with its exit code.
- `--profile FILE` given before NAME is passed to the program as the
  environment variable `FHIR_PIPES_PROFILE`, the variable `--profile`
  reads when it is not given. A `FHIR_PIPES_PROFILE` already set is passed
  on unchanged. `--profile` after NAME is an argument of the program.
- When no program is found, fhir-pipes prints its usual error for an
  unknown command, then a line saying that `fhir-pipes-NAME` was not found,
  and exits 2.

NAME may hold letters, digits, `-` and `_`.

## dhis2 extract

`fhir-pipes dhis2 extract` is the DHIS2 connector. It writes the records
of the package
[examples/import/icr-dhis2](../examples/import/icr-dhis2):

```sh
fhir-pipes dhis2 extract locations --instance sl-demo --id-prefix sle --country-code SLE --out raw
fhir-pipes dhis2 extract measures --instance sl-demo --indicator-group srxUNt2w5jn --out raw
fhir-pipes dhis2 extract reports --instance sl-demo --indicator-group srxUNt2w5jn \
  --period 2025 --level 2 --level 3 --out raw
fhir-pipes import run --package examples/import/icr-dhis2 --in raw --out fhir
```

Each run writes one file in `raw/`: `DHIS2OrgUnit.ndjson`,
`DHIS2Indicator.ndjson` and `DHIS2Value.ndjson`. See
[dhis2.md](dhis2.md#the-records) for the options.

The records carry what DHIS2 returns and what is worked out from it, and
none of the URLs the resources have: those are parameters of the import
package. `extract reports` finds each org unit in the `DHIS2OrgUnit` records
that `extract locations` wrote to the same folder, not in FHIR Locations.
So `dhis2 extract` reads no profile, and one `import run` maps the whole
folder.

`dhis2 locations`, `dhis2 measures` and `dhis2 reports` run the same
extraction and map each record with a package as it is read, for those who
want one command (see
[dhis2.md](dhis2.md#the-commands-that-write-resources)).

`dhis2 extract` is built into fhir-pipes for now. It will move to its own
program, `fhir-pipes-dhis2`; the same command line,
`fhir-pipes dhis2 extract …`, will then run that program as an external
command.
