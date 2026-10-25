#!/usr/bin/env python3
"""Exercise the offline/pinned tokenizer build-input boundary without network."""

from __future__ import annotations

import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("fetch_tokenizer_assets", ROOT / "scripts/fetch-tokenizer-assets.py")
assert SPEC is not None and SPEC.loader is not None
FETCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FETCH)


class TokenizerAssetTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.payload = b'{"model":{"type":"BPE","vocab":{}}}\n'
        digest = hashlib.sha256(self.payload).hexdigest()
        self.asset = {
            "id": "fixture_model",
            "filename": digest + ".json",
            "format": "huggingface_json",
            "sha256": digest,
            "size_bytes": len(self.payload),
            "sources": [{
                "repository": "fixture/model",
                "revision": "a" * 40,
                "url": "https://huggingface.co/fixture/model/resolve/" + "a" * 40 + "/tokenizer.json",
            }],
        }
        self.manifest = self.root / "manifest.json"
        self.write_manifest()
        self.output = self.root / "assets"

    def write_manifest(self) -> None:
        self.manifest.write_text(json.dumps({"schema_version": 1, "assets": [self.asset]}), encoding="utf-8")

    def test_checked_in_manifest_deduplicates_eight_models_with_pinned_sources(self) -> None:
        assets = FETCH.load_manifest(FETCH.MANIFEST)
        self.assertEqual(len(assets), 6)
        self.assertEqual(sum(len(asset["sources"]) for asset in assets), 8)
        self.assertEqual(sum(asset["size_bytes"] for asset in assets), 106627804)
        self.assertEqual({asset["format"] for asset in assets}, {"huggingface_json", "tiktoken_ranks"})
        self.assertTrue(all(source["license"] and source["license_url"] for asset in assets for source in asset["sources"]))

    def test_fetch_verifies_before_atomic_install_and_reuses_offline(self) -> None:
        with patch.object(FETCH, "urlopen", return_value=io.BytesIO(self.payload)) as request:
            directory = FETCH.prepare(self.manifest, self.output)
        self.assertEqual(request.call_count, 1)
        self.assertEqual(directory, self.output.resolve())
        self.assertEqual((directory / self.asset["filename"]).read_bytes(), self.payload)
        with patch.object(FETCH, "urlopen", side_effect=AssertionError("offline verification attempted network")):
            FETCH.prepare(self.manifest, self.output, check_only=True)
            FETCH.prepare(self.manifest, self.output)
        self.assertEqual(list(directory.iterdir()), [directory / self.asset["filename"]])

    def test_corrupt_existing_cache_fails_without_network_or_replacement(self) -> None:
        self.output.mkdir()
        artifact = self.output / self.asset["filename"]
        artifact.write_bytes(b"x" * len(self.payload))
        with patch.object(FETCH, "urlopen", side_effect=AssertionError("corrupt cache must fail closed")):
            with self.assertRaisesRegex(ValueError, "hash mismatch"):
                FETCH.prepare(self.manifest, self.output)
        self.assertEqual(artifact.read_bytes(), b"x" * len(self.payload))

    def test_invalid_download_never_becomes_an_asset(self) -> None:
        for payload in (b"short", b"x" * len(self.payload), self.payload + b"too large"):
            with self.subTest(size=len(payload)):
                with patch.object(FETCH, "urlopen", return_value=io.BytesIO(payload)):
                    with self.assertRaisesRegex(ValueError, "could not fetch a hash-verified"):
                        FETCH.prepare(self.manifest, self.output)
                self.assertEqual(list(self.output.iterdir()), [])

    def test_missing_offline_asset_does_not_create_directory_or_fetch(self) -> None:
        with patch.object(FETCH, "urlopen", side_effect=AssertionError("check must be offline")):
            with self.assertRaisesRegex(ValueError, "missing tokenizer asset"):
                FETCH.prepare(self.manifest, self.output, check_only=True)
        self.assertFalse(self.output.exists())

    def test_manifest_rejects_mutable_urls_bad_hashes_paths_and_duplicates(self) -> None:
        mutations = (
            ("filename", "../tokenizer.json"),
            ("filename", "tokenizer.json"),
            ("sha256", "0" * 63),
            ("format", "model_weights"),
            ("size_bytes", FETCH.MAX_ASSET_BYTES + 1),
            ("size_bytes", True),
        )
        for name, value in mutations:
            original = self.asset[name]
            with self.subTest(field=name, value=value):
                self.asset[name] = value
                self.write_manifest()
                with self.assertRaises(ValueError):
                    FETCH.load_manifest(self.manifest)
                self.asset[name] = original
        source = self.asset["sources"][0]
        original_url = source["url"]
        for url in (
            original_url.replace("/" + "a" * 40 + "/", "/main/"),
            original_url + "?token=secret",
            original_url.replace("https:", "http:"),
            original_url.replace("huggingface.co", "unreviewed.example"),
            original_url.replace("tokenizer.json", "model.safetensors"),
        ):
            with self.subTest(url=url):
                source["url"] = url
                self.write_manifest()
                with self.assertRaises(ValueError):
                    FETCH.load_manifest(self.manifest)
        source["url"] = original_url
        self.manifest.write_text(json.dumps({"schema_version": 1, "assets": [self.asset, self.asset]}))
        with self.assertRaisesRegex(ValueError, "duplicate"):
            FETCH.load_manifest(self.manifest)

    def test_backend_build_embeds_from_fixed_output_manifest(self) -> None:
        nix_assets = (ROOT / "crates/model-tokenizer/nix/assets.nix").read_text()
        self.assertIn("../assets/manifest.json", nix_assets)
        self.assertIn("sha256 = asset.sha256", nix_assets)
        source = (ROOT / "services/opensecret/flake.nix").read_text()
        self.assertIn("../../crates/model-tokenizer/nix/assets.nix", source)
        self.assertIn("../../crates/model-tokenizer\n", source)
        self.assertGreaterEqual(source.count("MAPLE_TOKENIZER_ASSETS_DIR = tokenizerAssets"), 2)


if __name__ == "__main__":
    unittest.main()
