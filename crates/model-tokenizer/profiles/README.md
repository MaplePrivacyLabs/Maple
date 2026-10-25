# Pinned text prompt profiles

The renderer identity is deliberately separate from the public model ID and
from the raw tokenizer asset. `manifest.json` records the complete source URL,
revision, SHA-256 and upstream license for every bundled Jinja file. The files
are copied without modification. The crate's integration license does not
replace their upstream licenses.

| Profile | Source and qualification boundary |
| --- | --- |
| GLM 5.3 official | Z.AI `ba32f8c`, template `0e2b6268`; custom GLM-5.3 license included under `licenses/` |
| GLM 5.3 Flash official | Z.AI `46a050b`, template `2614a3c8`; MIT license included under `licenses/` |
| GLM 5.3 Tinfoil NVFP4 | Inferact `ce67b36`, template `69bb3ab5`; its upstream license is included under `licenses/` |
| GLM 5.3 Continuum | ConfigMap-enforced `3740abce` recovered byte-for-byte from official Z.AI `c5087a76`; vLLM 0.31 with forced string content |
| GLM 5.3 Flash Continuum | ConfigMap-enforced `0c4099f3` recovered byte-for-byte from official Z.AI `91b69215`; vLLM 0.31 auto-detects OpenAI content arrays |
| GLM 5.3 Flash Tinfoil NVFP4 | RedHatAI `240131d6` ships the same `0c4099f3` template, but this recipe forces string content; its profile remains distinct |
| GPT-OSS / Safeguard Jinja references | OpenAI `b5c939d` / `3c73911`, Apache-2.0; **native vLLM Harmony serving bypasses these templates** |
| GPT-OSS / Safeguard native Harmony | Tinfoil's vLLM `0b3ba88` and Harmony `abd677f`; native segmented encoding and TypeScript tool schemas, independently tested against the pinned Python implementation |
| Kimi K3 native serving | Tinfoil's vLLM `db9527a` string-content parser and native Kimi wrapper, then Moonshot `a590ce0`'s separately tested XTML encoder |
| Gemma 4 Tinfoil source recipe | vLLM `db9527a`, template `afdbb2ab`, Apache-2.0; this is the recipe's override, not Google's canonical template |
| Llama 3.3 Tinfoil source recipe | vLLM `0b3ba88`, template `c22af47e`, Apache-2.0; this is the recipe's override, not Meta's canonical template |

The GLM official and provider templates differ despite sharing the same raw
tokenizer. Privatemode's published deployment pins its ConfigMap templates by
hash. Those files are absent from its GitHub tree, but their exact bytes are
available in official Hugging Face history. `manifest.json` binds the enforced
deployment hashes to those immutable source revisions. No current-head template
is substituted for the matching historical file.

The Tinfoil Flash tokenizer JSON differs from the shared canonical JSON only
in its truncation configuration. The complete decoded JSON was compared:
vocabulary, merges, pre-tokenizer, normalization, added tokens and every other
field are identical. Both encoders disable padding/truncation. The verified
file hashes and normalized equivalence receipt are retained in
`glm53-flash-tokenizer-equivalence.json`, so this route reuses the same raw
engine while preserving its own template/content-format identity.

The text preprocessor follows pinned vLLM `db9527a` for the current reference
profiles and `0b3ba88` for the older Llama source recipe. These versions differ:
the older Llama path preserves valid array/scalar historical tool arguments and
Pydantic extra fields, rejects malformed argument JSON, and does not convert
developer messages to system messages. Both paths match their content-array
format, tool-result text joining, reasoning aliases and function default/order
rules. Historical JSON-string argument key order is preserved locally without
enabling a global serde map-order feature. This identifies the source profile
being tested; it does not prove a third-party provider runs that exact
normalizer or template.

`tests/generate_hf_chat.py` creates synthetic rendered-string fixtures using
Python Jinja2 3.1.6 / Transformers 4.57.6 and the pinned local files. Generation
is an offline developer operation, not a runtime dependency. Rust tests compare
complete rendered bytes, including whitespace, Unicode, tools, reasoning,
special-marker literals, dates and generation prefixes. The matrix includes
both provider content formats, tool-result ordering, reasoning replay,
clear-thinking and thinking-enable options. The raw tokenizer
engine has separate token-ID parity fixtures. Live-provider usage observations
remain a separate evidence layer.

No media expansion is approximated with visible placeholder text: image,
audio, video and embedding inputs return a typed unsupported result. Extra
tokenizer truncation/BOS overrides and custom templates are also unsupported.
No runtime network fetch or Python/Go daemon is introduced.

Kimi's serving profile is separate from direct official-encoder fixtures. Its
vLLM boundary joins nonempty text parts, resolves deprecated reasoning aliases,
applies typed tool defaults, decodes historical JSON arguments once, and handles
synthetic tool-result IDs. Parsed argument object order and duplicate-key
semantics are preserved with a local Python-compatible JSON string bridge;
there is no process-wide map-order change. The native thinking aliases preserve
the upstream setdefault precedence. `generate_kimi_serving.py` executes the
hash-checked pinned upstream AST functions, actual Pydantic serializers and
official encoder to produce independent segment goldens. This source-function
test does not instantiate the full HTTP request validation or certify a mutable
live endpoint.

`RenderContext` captures a UTC date once. Callers caching counts must include
the profile, date and complete relevant request/options in the cache identity.
No per-conversation stored tokenizer or duplicated history is required.
