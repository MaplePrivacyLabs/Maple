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

The port is in progress. The harness skeleton does not implement the selected
runtime modules. The authoritative progress records are
[`upstream-map.toml`](../pi-conformance/coverage/upstream-map.toml) for individual
tests and [`corpus-status.toml`](../pi-conformance/coverage/corpus-status.toml)
for scenarios and function goldens. Pending entries are not evidence of parity.

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

## Updating the pin

Keep the source pin fixed while this port is implemented. For a subsequent
update, run the selection scripts against both tags, review changed slices and
their dependency closure, translate changed tests, and regenerate the corpus
with the isolated reference flake. Do not edit expected corpus output by hand.
Behavioral differences belong in the deviation registry with owner approval;
ownership or interface adaptations belong in the source manifest.
