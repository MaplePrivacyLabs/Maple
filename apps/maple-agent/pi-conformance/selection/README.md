# Pi source selection audit

These Python standard-library tools describe the Rust port's source scope at
Pi `v1.0.4`, revision `7c10bd4337495ee613f2224843ecdf349b80d1df`.
They read an upstream checkout and write audit results to paths you supply.
They do not install dependencies, execute TypeScript, or contact providers.

`classes.py` is the source selection: 81 files across `pi-agent-core`, `pi-ai`,
`pi-coding-agent`, and the `pi-mcp` content-conversion slice. Each entry records
its class, crate, SHA-256 of the full pinned upstream file, exact `rust_module`
relative to `apps/maple-agent` (for example `crates/pi-ai/src/utils/json_parse.rs`),
selection cuts, and an
`adaptations` list. The Rust mapping preserves the upstream `src/` directory
structure with snake_case filenames. Empty adaptation lists await implementation
annotations; they do not assert that a selected `Adapt` file is unchanged.
`selection.txt` contains the same 81 paths, including the MCP slice, for consumers
that need a simple list. `manifest.json` exports all selection metadata with an
explicit implementation status. Its initial `pending` entries are mappings to
validate during the port, not claims that Rust modules already exist.

From the `pi-conformance` directory, set `PI_ROOT` to an upstream checkout at
the revision above, then run:

```sh
audit_dir="$(mktemp -d)"
python3 -I -B selection/inv.py "$PI_ROOT" "$audit_dir/inventory.json" --with-tests
python3 -I -B selection/measure.py "$PI_ROOT" "$audit_dir/inventory.json" "$audit_dir/measured.json"
python3 -I -B selection/closure.py "$audit_dir/inventory.json" selection/selection.txt --npm > "$audit_dir/closure.txt"
python3 -I -B selection/deps.py "$PI_ROOT" "$audit_dir/inventory.json" "$audit_dir/dependencies.json"
python3 -I -B selection/tests.py "$PI_ROOT" "$audit_dir/inventory.json" selection/selection.txt "$audit_dir/tests.json"
python3 -I -B selection/testcurate.py "$audit_dir/tests.json" "$audit_dir/curated-tests.md"
python3 -I -B selection/generate-test-files.py --check --upstream-root "$PI_ROOT"
```

At the pinned revision, measurement yields 500 source rows: 81 selected and 419
excluded, with no unmatched files or missing cut names. The selected slices total
27,107 physical lines and an estimated 20,383 code lines. `measure.py` counts
explicit kept ranges exactly and prorates shared header code for member cuts.
The static test audit finds 480 test files; the curated table contains 144 files.
Static call counts do not expand parameterized cases or establish passing tests.
The harness's collected runtime inventory supplies actual case identities and
execution status.

`coverage-overrides.json` records case-level decisions for partial files using
the exact runtime IDs. Purely excluded behavior has a fixed reason code; mixed
cases remain pending with an adaptation describing the retained assertions.
The initial coverage generator rejects unknown or duplicate override IDs.
It generates `coverage/upstream-map.toml` once; implementation statuses in that
map are then maintained as Rust tests are completed.

`testcurate.py` is safe to import and owns the curated test-file classifications:
`T` (whole file), `P` (selected cases), `E` (live-provider cases), `R` (reference
extension), and `A` (host transport adapter). Run `generate-test-files.py` after
editing this table or `classes.py` to regenerate `test-files.json` and
`manifest.json`; `--check` fails if either output has drifted. The generator
also rejects duplicate Rust mappings or differences between `selection.txt` and
`classes.py`. Pass `--upstream-root` to additionally check the full upstream file
bytes against every recorded SHA-256; this works with both Git checkouts and
Nix source trees. Without it, `--check` verifies metadata and generated-file drift.
Record `status="ported"` or `status="adapted"` in a source entry only
after its Rust implementation has been validated.
The JSON is sorted, contains no host paths, and can be consumed without Python.

Supporting scripts are included so this directory is self-contained:
`tslex.py` and `members.py` provide lexical counting and declaration ranges;
`testclass.py` provides a heuristic test classification; `describes.py` summarizes
static describe blocks; `steps.py` and `steptests.py` audit dependency ordering.
`calib.py` and `compare.py` retain a fully specified historical baseline for
comparison, which is distinct from the current selected scope.

Dependency closure is a lexical audit, not a TypeScript compiler. It reports
imports from selected source files before Rust adaptations, so excluded host
services and type-only dependencies can still appear. `deps.py` further separates
references in kept and cut members. Quoted command names and prose containing
`import` are ignored by import extraction.
