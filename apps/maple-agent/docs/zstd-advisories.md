# Native Agent zstd advisory exceptions

The exceptions in `../deny.toml` apply to the 14 findings below, reviewed against
`zstd 0.13.3`, `zstd-safe 7.2.4`, and `zstd-sys 2.1.0+zstd.1.5.7` on the native
macOS, Linux, and Windows targets. Other vulnerability/unsoundness findings and
the package bans remain blocking. This policy does not apply to Maple Research's
separate graph.

These are real upstream defects. Source review found no triggering path in the
Agent's selected APIs, features, context state, and native platforms. This is a
bounded reachability assessment, not an exploit test or a claim that all uses of
these dependency versions are safe. The patched upstream versions are
`zstd 0.14.3`, `zstd-safe 8.2.0`, and `zstd-sys 2.1.2`.

## Reviewed consumers

The Agent lockfile has exactly two direct zstd consumers and no independent
zstd-safe or zstd-sys consumer:

- [`store/codec.rs`](../crates/maple-agent/src/store/codec.rs) uses
  `stream::encode_all(bytes, 3)` and `stream::read::Decoder::new`, with a bounded
  read into a separate caller-owned vector. Each operation creates a fresh owned
  context. Attachment-migration tests also use `encode_all`.
- CUA `cursor-overlay`, pinned at
  [`2a3975570a885f45ef83f7e81bba54a01804ebd0`](https://github.com/trycua/cua/blob/2a3975570a885f45ef83f7e81bba54a01804ebd0/libs/cua-driver/rust/crates/cursor-overlay/src/theme_artifact.rs#L294-L350),
  uses `encode_all` at level 12 for theme authoring and `read::Decoder` for theme
  loading. Custom themes can be external files; the assessment does not assume
  that compressed inputs are trusted.

Both dependencies request ordinary zstd features. `experimental`, `seekable`,
and `zstdmt` are not requested. Dictionary-building support is enabled by
default, but neither consumer calls its training API. The selected decoding
adapter creates valid input buffers and initialized slice output at position
zero; it does not supply an uninitialized caller vector to zstd-safe. Encoding
uses its internal output vector safely at position zero.

## Finding-specific rationale

The links pin the reviewed RustSec database snapshot, rather than treating an
advisory's package-version match as proof of a reachable product defect.

| Advisory | Required path or state absent from the reviewed Agent |
| --- | --- |
| [0391](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd/RUSTSEC-2026-0391.md) | Borrowed `with_context` reuse. Both consumers create fresh owned contexts; `encode_all` finishes its stream. |
| [0395](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd-safe/RUSTSEC-2026-0395.md) | Enabling `StableInBuffer`. The ordinary encoders select only the compression level. |
| [0396](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd-safe/RUSTSEC-2026-0396.md) | Seekable decoding or seek-table access. These APIs and the feature are unused. |
| [0397](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd-safe/RUSTSEC-2026-0397.md) | `OutBuffer::around_pos` with a gap beyond an initialized vector. The selected adapters use `around` at position zero. |
| [0398](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd-safe/RUSTSEC-2026-0398.md) | Seekable decompression with a caller-selected offset. Neither consumer uses it. |
| [0399](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd-safe/RUSTSEC-2026-0399.md) | Experimental borrowed prepared-dictionary initialization. Ordinary wrappers load empty dictionary bytes into fresh contexts instead. |
| [0400](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd-safe/RUSTSEC-2026-0400.md) | Mixing low-level one-shot methods with an unfinished stream on the same context. `encode_all` is internally streaming and does not do this. |
| [0401](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd-safe/RUSTSEC-2026-0401.md) | Dictionary training using untrusted sample-size arrays. The available training API is never called. |
| [0402](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd-safe/RUSTSEC-2026-0402.md) | Experimental `CCtx::try_clone` with multithreaded context state. No clone or `NbWorkers` setting is used. |
| [0403](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd-safe/RUSTSEC-2026-0403.md) | The generic decompression method is reached, but the vulnerable vector-output path is absent. The advisory explicitly excludes `read::Decoder`; its reader uses an initialized slice and propagates errors before returning written bytes. |
| [0404](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd-safe/RUSTSEC-2026-0404.md) | Experimental `DCtx::try_clone`, dictionary/prefix cloning, or mid-frame cloning. These APIs are unused. |
| [0405](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd-safe/RUSTSEC-2026-0405.md) | Seekable compression with an invalid caller-selected input-buffer position. The seekable API is unused. |
| [0406](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd-safe/RUSTSEC-2026-0406.md) | Changing a context with an unfinished multithreaded frame. Settings are applied only to fresh single-worker contexts before streaming. |
| [0410](https://github.com/RustSec/advisory-db/blob/a78282a1414d6c28cc228c4eae3570451affb0dc/crates/zstd-sys/RUSTSEC-2026-0410.md) | The affected allocator module is compiled only for `wasm32`. The supported Agent targets are native. |

## Re-review and removal

Re-review the complete exception list whenever either consumer, the CUA revision,
the resolved compression versions/features, or the supported target list changes.
In particular, borrowed context reuse, write-side decoders, dictionary training,
prepared dictionaries, seekable/experimental APIs, context cloning, worker
configuration, or a Wasm target invalidate the relevant reasoning above.

Remove each exception once all affected locked packages have been upgraded to
patched versions. Run `scripts/audit-sqlcipher-dependencies.py` against a current
advisory database for each supported target after changing this policy. Keep new
advisories blocking; these entries do not authorize accepting future findings.
