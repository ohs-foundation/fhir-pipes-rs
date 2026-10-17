# Profiles

A profile is a TOML file that describes an implementation guide (IG) to
fhir-pipes: the extra Location and Organization columns, the admin units and
type codes, and the URLs that `dhis2`, `population` and `import` put on new
resources. Without one, fhir-pipes reads base FHIR R4 only.

## Giving a profile

In order of precedence:

1. `--profile FILE|URL` (https only for URLs).
2. `FHIR_PIPES_PROFILE`.
3. For `refresh`, `profile = "…"` in `fhir-pipes.toml`, relative to the file.
4. For a command on a snapshot, the `profile.toml` copy that
   `transform locations` or `run` left beside it.

Each command that uses a profile prints it on its first line, for example
`profile: icr (examples/profiles/icr.toml)`. ICR's profile is
`examples/profiles/icr.toml` in the fhir-pipes repository.

## Which commands need one

| Command | Without a profile |
|---|---|
| `transform locations`, `run`, `refresh` locations step | built-in columns only; `country` is `unknown` unless `--country` |
| `diff` | writes back built-in columns; refuses edits to other columns |
| `index` | refuses (needs `spatial_index_extension`) |
| `population` | refuses (needs `[population]`) |
| `dhis2 …` | refuses (needs `[dhis2]`) |
| `import location-boundaries`, `import location-sites` | refuses (needs the Location type system) |
| `extract`, `export`, `load`, `sync`, `views` | not used |

A refusal exits 2 with a message that ends "Pass --profile; see docs/profiles.md".

## Checking a profile against its IG

```sh
fhir-pipes profile check --profile ig/fhir-pipes-profile.toml --ig ig/fsh-generated/resources
fhir-pipes profile check --profile examples/profiles/icr.toml --ig https://icr.healthcampaigns.org/package.tgz
```

`--ig` is a FHIR package (`package.tgz`, a path or an https URL) or a
directory of JSON resources such as SUSHI's `fsh-generated/resources`. It
reports extensions, code systems, profiles and identifier systems the
profile names that the IG lacks or defines differently, as errors or
warnings. It exits 1 on any error, so it can run in the IG's CI.

## Common profile errors

| Message | Meaning |
|---|---|
| `profile …: unknown field …` | a key that does not exist, often a typo |
| `… needs a profile with a [dhis2] / [population] section` | pass a profile that has it |
| `index needs a profile that names … spatial_index_extension` | pass a profile that names it |
| `profiles are fetched over https only` | use an https URL or a local file |
| `warning: --profile … differs from …/profile.toml` | the snapshot was transformed with another profile |
