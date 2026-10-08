# Pi source correspondence

The `pi-ai`, `pi-agent-core`, `pi-coding-agent`, and `pi-mcp` crates track
[Pi v1.0.4](https://github.com/earendil-works/pi/tree/v1.0.4), commit
`7c10bd4337495ee613f2224843ecdf349b80d1df`. `PI-LICENSE` contains the
upstream MIT license. `pi-testkit` and `pi-conformance` are unpublished test
support crates.

The source selection and Rust module mapping live in
[`../pi-conformance/selection/manifest.json`](../pi-conformance/selection/manifest.json).
The accompanying selection scripts calculate source slices, dependencies, and
test associations from an upstream checkout. Each implemented file records its
Rust ownership, async, or host-interface adaptations there.

The port is in progress. Step 1 now has implementations for its 19 selected
`pi-ai` files: contracts, model helpers, session resources, utilities and the
scripted provider. The other 62 selected source files remain pending. Rust-only
`js_value`, `js_string`, `js_json`, `js_serde`, `js_deserialize` and
`raw_message` modules support
JavaScript binary64 values, UTF-16 strings, ordered properties, field presence
and serialization; their correspondence is recorded with the selected types.

The nine Step 1 upstream test files contain 148 collected cases. The coverage
map retains four exclusions and maps all 144 selected cases to distinct compiled
Rust tests: 61 ported and 83 adapted. Additional Rust regression tests and three
supplementary catalog-fixture tests do not increase that upstream coverage count.
The authoritative progress records are
[`upstream-map.toml`](../pi-conformance/coverage/upstream-map.toml) for individual
tests and [`corpus-status.toml`](../pi-conformance/coverage/corpus-status.toml)
for scenarios and function goldens. Source implementation, translated tests and
corpus replay are separate evidence; pending entries are not evidence of parity.

Step 1 currently measures **9,770 production Rust code lines** against the
initial 3,100–3,800 estimate: 7,791 in the 19 mapped source files and 1,979 in
the six JavaScript representation and access support modules. This count excludes blank
and comment-only lines, trailing `#[cfg(test)]` modules, integration tests,
module declarations, the existing `PiEnv` seam and conformance infrastructure.
The larger implementation includes a local TypeBox-compatible conversion and
error layer, a partial JSON parser, explicit async/shared-state machinery, and
JavaScript representation and serde support that the estimate understated.
TypeBox attribution is in
[`pi-ai/THIRD-PARTY-NOTICES.md`](pi-ai/THIRD-PARTY-NOTICES.md).

## Boundaries

The production crates have no dependency on a Maple crate. Host model routing,
authentication, transport, tools, and MCP connections are supplied at the
integration boundary. The reference source, Node runtime, and generated model
catalog belong only to the isolated conformance flake.

Pi reads ambient clocks, timers, random values, and IDs. The Rust port injects
these through `pi_ai::env::PiEnv`; its test implementation is
`pi_testkit::VirtualEnv`. Each test owns its state. The custom upstream UUIDv7
algorithm remains a separate source port, rather than being replaced by a
generic UUID generator.

Third-party Rust dependencies are permitted. The conformance gate rejects local
dependencies outside the six Pi crates and any reachable Maple package,
including transitive dependencies. Test support must remain a dev-dependency of
production crates. Dependency updates must preserve translated tests and the
recorded corpus. JSON Schema evaluation must disable network and filesystem
retrieval explicitly, even when another workspace member enables optional
library features.

## Provisional ownership boundary

Tool definitions and provider request/configuration inputs use owned Rust
values. Changing an original definition after inserting or replaying it does
not mutate the stored message. This engineering choice limits shared mutable
state in the native API while keeping shared streaming messages and explicit
mutable hook arguments/results. Outer request changes use returned replacements.
The owner authorized continuing with this decision on 2026-10-08 and may revisit
it after planning review.

The `transcript.toolOwnership` paired fixture records both source alias cases.
Its narrow authorized rule asserts Pi's exact mutation and Rust's retained copy;
it does not ignore unrelated fields. The rule and authorization live in
[`deviations.toml`](../pi-conformance/coverage/deviations.toml). Immediate-consumer
stream scheduling is a separate question, not covered by this decision.

## Updating the pin

Keep the source pin fixed while this port is implemented. For a subsequent
update, run the selection scripts against both tags, review changed slices and
their dependency closure, translate changed tests, and regenerate the corpus
with the isolated reference flake. Do not edit expected corpus output by hand.
Behavioral differences belong in the deviation registry with owner approval;
ownership or interface adaptations belong in the source manifest.
