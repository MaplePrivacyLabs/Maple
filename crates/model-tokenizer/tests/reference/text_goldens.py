#!/usr/bin/env python3
"""Regenerate synthetic text-token goldens with independently pinned Python engines.

No network, GPU, model weights or customer content. Pass the verified offline
asset directory created by the repository fetcher/Nix. Rust tests consume the
checked-in JSON and have no Python dependency.
"""

import argparse
import base64
import hashlib
import importlib.metadata
import json
import struct
from pathlib import Path

import tiktoken
from tokenizers import Tokenizer
from kimi_tokenization_reference import ReferenceTokenizer

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "assets/manifest.json"
CASES = [
    ("empty", ""),
    ("english", "The quick brown fox reviewed the cached model responses."),
    ("contractions", "We're checking tokenization: it's precise, isn't it? I've tested it."),
    ("unicode", "日本語の文章 中文测试 العربية हिन्दी Español café naïve Σίσυφος 🌲🚀"),
    ("normalization", "é e\u0301 ① Ⅳ ﬃ ＡＢＣ ～ \u00a0\u2028\u200d"),
    ("whitespace", "\t  first\r\n\nsecond   \n\tend  "),
    ("numbers", "0 123 1234 1234567890 -42 3.1415926535 0xDEADBEEF"),
    ("rust", 'fn main() {\n    let data = [0u64, 42, 9001];\n    println!("{:?}", data);\n}\n'),
    ("json", '{"tool":"search","arguments":{"query":"hello 世界","limit":10}}'),
    ("specials", "[BOS]<|open|> user <|sep|> <|end_of_msg|> <|start|> <|im_end|> <｜begin▁of▁sentence｜> <|endoftext|>"),
    ("reserved", "<|reserved_token_163620|> <|reserved_special_token_25|>"),
    ("long_identifier", "identifier_without_spaces_" * 80),
]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--assets-dir", required=True, type=Path)
    args = parser.parse_args()
    versions = {package: importlib.metadata.version(package) for package in ["tokenizers", "tiktoken"]}
    if versions != {"tokenizers": "0.22.2", "tiktoken": "0.14.0"}:
        raise SystemExit("Use pinned Python reference tokenizers0.22.2/tiktoken0.14.0")
    manifest = json.loads(MANIFEST.read_text())
    config_bytes = (ROOT / "assets/kimi-k3-tokenizer-config.json").read_bytes()
    assert hashlib.sha256(config_bytes).hexdigest() == "5d0803c94db9cd78763499e0956c95fd5a225c14a727e5a6cf5db3f96f010a6e"
    config = json.loads(config_bytes)
    engines = {}
    for asset in manifest["assets"]:
        path = args.assets_dir / asset["filename"]
        raw = path.read_bytes()
        assert len(raw) == asset["size_bytes"]
        assert hashlib.sha256(raw).hexdigest() == asset["sha256"]
        if asset["format"] == "huggingface_json":
            normal = Tokenizer.from_file(str(path))
            normal.no_truncation()
            normal.no_padding()
            ordinary = Tokenizer.from_file(str(path))
            ordinary.no_truncation()
            ordinary.no_padding()
            ordinary.encode_special_tokens = True
            engines[asset["id"]] = (
                lambda text, tokenizer=normal: tokenizer.encode(text, add_special_tokens=False).ids,
                lambda text, tokenizer=ordinary: tokenizer.encode(text, add_special_tokens=False).ids,
            )
        else:
            ranks = {base64.b64decode(token): int(rank) for token, rank in map(bytes.split, raw.splitlines())}
            specials = {config["added_tokens_decoder"].get(str(i), {}).get("content", f"<|reserved_token_{i}|>"): i for i in range(len(ranks), len(ranks) + 256)}
            tokenizer = ReferenceTokenizer()
            tokenizer.model = tiktoken.Encoding(name="kimi_k3_pinned", pat_str=ReferenceTokenizer.pat_str, mergeable_ranks=ranks, special_tokens=specials)
            engines[asset["id"]] = (
                lambda text, tokenizer=tokenizer: tokenizer._encode_text_piece(text, allow_special_tokens=True),
                lambda text, tokenizer=tokenizer: tokenizer._encode_text_piece(text, allow_special_tokens=False),
            )
    output = {
        "reference": {"synthetic_only": True, "packages": versions, "kimi_source_revision": "a590ce090cb049c93a33dfe8c208ec652aa20503", "manifest_sha256": hashlib.sha256(MANIFEST.read_bytes()).hexdigest()},
        "fixtures": [{"name": name, "text": text, "expected": {asset: {"raw": normal(text), "ordinary": ordinary(text)} for asset, (normal, ordinary) in engines.items()}} for name, text in CASES],
    }
    destination = ROOT / "tests/fixtures/text_goldens.json"
    destination.write_text(json.dumps(output, indent=2, ensure_ascii=False) + "\n")
    print(f"Wrote {len(CASES)} fixtures × {len(engines)} assets × 2 special-token policies")
    boundaries = [
        ("marker_at_25k_ascii_run", "a", 24_999, "<|open|>"),
        ("marker_at_25k_unicode_run", "界", 24_999, "<|open|>"),
        ("whitespace_at_25k", " ", 25_001, "tail"),
        ("marker_at_400k_outer", "x ", 199_999, "a<|open|>"),
        ("python_information_separator", "a", 24_999, "\u001c<|open|>"),
    ]
    normal, ordinary = engines["kimi_k3"]
    def expected(encoder, text):
        ids = encoder(text)
        return {"count": len(ids), "ids_le_sha256": hashlib.sha256(b"".join(struct.pack("<I", id) for id in ids)).hexdigest()}
    output = {"reference": output["reference"], "fixtures": [{"name": name, "unit": unit, "repeats": repeats, "suffix": suffix, "raw": expected(normal, unit * repeats + suffix), "ordinary": expected(ordinary, unit * repeats + suffix)} for name, unit, repeats, suffix in boundaries]}
    (ROOT / "tests/fixtures/kimi_boundaries.json").write_text(json.dumps(output, indent=2, ensure_ascii=False) + "\n")
    print(f"Wrote {len(boundaries)} Kimi wrapper boundary token-ID hash fixtures")


if __name__ == "__main__":
    main()
