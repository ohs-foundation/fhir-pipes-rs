# fhirpath.js test cases

fhirpath.js's FHIRPath tests, run by `tests/fhirpath_js.rs`.

- `cases/`: <https://github.com/HL7/fhirpath.js>, `test/cases/*.yaml`, commit
  `137c5014cb0fb7ba66c5762ff40bd5a3a000fd48` (2026-10-06), converted to JSON
  so the tests need no YAML parser. The conversion changes no values: no
  scalar in these files reads differently under YAML 1.1 (PyYAML) and the
  YAML 1.2 core schema (js-yaml). Left out: `fhir-r4.yaml`, fhirpath.js's
  copy of the HL7 R4 suite, which `tests/fhirpath_hl7.rs` runs from the XML.
- `resources/`: the input files the cases name, from `test/resources/` at the
  same commit, as they are. `fhir-r5.yaml` names `r5/ccda.json`, which
  fhirpath.js does not ship; its tests do not run.
- `LICENSE.md`: fhirpath.js's license, which redistribution must keep.
- `known-failures.json`: the tests fhir-pipes fails today, each with its
  cause (see `Cause` in `tests/common/mod.rs`), a note, and what the last run
  gave. The test writes `got`; the cause and note are written by hand.

To refresh, from a fhirpath.js checkout:

```
python3 - <fhirpath.js> crates/sof/tests/fhirpath-js <<'EOF'
import json, pathlib, shutil, sys, yaml
src, dest = map(pathlib.Path, sys.argv[1:])
inputs = set()
def walk(items):
    for t in items or []:
        if isinstance(t, dict):
            for k, v in t.items():
                if k.startswith('group'):
                    walk(v)
            if 'inputfile' in t:
                inputs.add((t.get('model') or 'r4', t['inputfile']))
for f in sorted((src / 'test/cases').glob('*.yaml')):
    if f.name != 'fhir-r4.yaml':
        data = yaml.safe_load(f.read_text())
        walk(data['tests'])
        (dest / 'cases' / (f.stem + '.json')).write_text(
            json.dumps(data, indent=2, ensure_ascii=False) + '\n')
for model, name in sorted(inputs):
    source = src / 'test/resources' / model / name
    if source.exists():
        (dest / 'resources' / model).mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, dest / 'resources' / model / name)
shutil.copyfile(src / 'LICENSE.md', dest / 'LICENSE.md')
EOF
```

then run `FHIRPATH_UPDATE_KNOWN_FAILURES=1 cargo test -p fhir-pipes-sof --test fhirpath_js`,
give each new failure a cause and note, and review the change to
`known-failures.json`.
