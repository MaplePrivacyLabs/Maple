#!/usr/bin/env python3
"""Regressions for target-aware keys and exact legacy identity migration."""

import copy
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import agent_cache
import agent_cache_scope as scope


APP = "apps/maple-agent/app/Cargo.toml"
MAC_DEPENDENCY = '\n[target.\'cfg(target_os = "macos")\'.dependencies]\nraw-window-handle = { version = "0.6", optional = true }\n'


class AgentCacheScopeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        for relative in ("apps/maple-agent/Cargo.toml", APP, "sdk/rust/Cargo.toml", "proxy/Cargo.toml"):
            path = self.root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('[package]\nname = "fixture"\nversion = "1.0"\nedition = "2024"\n', encoding="utf-8")
        self.app = self.root / APP
        self.app.write_text('[package]\nname = "fixture"\n[features]\ndefault = ["desktop"]\ndesktop = []\n', encoding="utf-8")
        with (self.root / "apps/maple-agent/Cargo.toml").open("a", encoding="utf-8") as manifest:
            manifest.write('\n[workspace]\nresolver = "2"\n')

    def identity(self, system="Windows"):
        host = {"Windows": "x86_64-pc-windows-msvc", "Linux": "x86_64-unknown-linux-gnu", "Darwin": "aarch64-apple-darwin"}[system]
        return {
            "schema": 1, "profile": "ci", "platform": system, "architecture": host.split("-")[0],
            "rustc": {"path": "/pinned/rustc", "version": f"rustc 1.98.0\nhost: {host}\nrelease: 1.98.0"},
            "native": {"compiler": "pinned", "sdk": "pinned"},
            "environment": {"RUSTFLAGS": "-D warnings", "CARGO_PROFILE_DEV_DEBUG": "0"},
            "configuration": {"files": {"guard": "original"}, "manifests": agent_cache.manifest_settings(self.root), "cargo_configs": {"workspace": {}}},
        }

    def key(self, identity):
        return agent_cache.digest(scope.canonical_identity(identity, self.root))

    def add_mac_dependency(self, edge="dep:raw-window-handle"):
        self.app.write_text(self.app.read_text().replace('desktop = []', f'desktop = ["{edge}"]') + MAC_DEPENDENCY, encoding="utf-8")

    def test_mac_only_optional_dependency_preserves_windows_and_linux_keys(self):
        before = {system: self.key(self.identity(system)) for system in ("Windows", "Linux", "Darwin")}
        self.add_mac_dependency()
        for system in ("Windows", "Linux", "Darwin"):
            with self.subTest(system=system):
                after = self.key(self.identity(system))
                self.assertEqual(after == before[system], system != "Darwin")

    def test_active_dependency_features_and_other_feature_edges_remain_guarded(self):
        self.add_mac_dependency()
        before = self.key(self.identity("Darwin"))
        self.app.write_text(self.app.read_text().replace('optional = true', 'optional = true, features = ["std"]'))
        self.assertNotEqual(self.key(self.identity("Darwin")), before)
        before = self.key(self.identity())
        self.app.write_text(self.app.read_text().replace('desktop = [', 'desktop = ["new-local-feature", '))
        self.assertNotEqual(self.key(self.identity()), before)

    def test_plain_optional_feature_and_strong_edge_are_conservative(self):
        for edge in ("raw-window-handle", "raw-window-handle/std"):
            with self.subTest(edge=edge):
                self.app.write_text('[package]\nname = "fixture"\n[features]\ndefault = ["desktop"]\ndesktop = []\n', encoding="utf-8")
                before = self.key(self.identity())
                self.add_mac_dependency(edge)
                self.assertNotEqual(self.key(self.identity()), before)
                canonical = scope.canonical_identity(self.identity(), self.root)
                settings = canonical["configuration"]["manifests"][APP]
                self.assertTrue(settings["dependency-features"])
                self.assertIn(edge, settings["features"]["desktop"])

    def test_explicit_local_feature_with_dependency_name_is_not_erased(self):
        self.add_mac_dependency()
        self.app.write_text(self.app.read_text().replace('[features]', '[features]\nraw-window-handle = []'))
        canonical = scope.canonical_identity(self.identity(), self.root)
        settings = canonical["configuration"]["manifests"][APP]
        self.assertIn("raw-window-handle", settings["features"])
        self.assertIn("dep:raw-window-handle", settings["features"]["desktop"])

    def test_weak_inactive_dependency_feature_is_removed_without_dropping_feature_name(self):
        self.add_mac_dependency()
        self.app.write_text(self.app.read_text().replace('desktop = ["dep:raw-window-handle"]',
                                                       'desktop = ["dep:raw-window-handle", "raw-window-handle?/std"]'))
        canonical = scope.canonical_identity(self.identity(), self.root)
        self.assertEqual(canonical["configuration"]["manifests"][APP]["features"]["desktop"], [])
        self.assertIn("desktop", canonical["configuration"]["manifests"][APP]["features"])

    def test_inactive_optional_dependency_implicit_local_feature_is_preserved(self):
        for edge in (None, "raw-window-handle?/std"):
            with self.subTest(edge=edge):
                self.app.write_text('[package]\nname = "fixture"\n[features]\ndefault = ["desktop"]\ndesktop = []\n', encoding="utf-8")
                before = self.key(self.identity())
                if edge is None:
                    self.app.write_text(self.app.read_text() + MAC_DEPENDENCY)
                else:
                    self.add_mac_dependency(edge)
                identity = self.identity()
                canonical = scope.canonical_identity(identity, self.root)
                self.assertNotEqual(agent_cache.digest(canonical), before)
                self.assertEqual(canonical["configuration"], identity["configuration"])

    def test_dependency_declared_active_or_unknown_elsewhere_keeps_feature_edges(self):
        self.add_mac_dependency()
        for table in ("dependencies", "target.'cfg(feature = \"custom\")'.dependencies"):
            with self.subTest(table=table):
                original = self.app.read_text()
                self.app.write_text(original + f'\n[{table}]\nraw-window-handle = {{version = "0.6", optional = true}}\n')
                canonical = scope.canonical_identity(self.identity(), self.root)
                self.assertIn("dep:raw-window-handle", canonical["configuration"]["manifests"][APP]["features"]["desktop"])
                self.app.write_text(original)

    def test_cfg_three_value_logic_preserves_unknowns(self):
        context = scope.native_target(self.identity())
        cases = {
            'cfg(target_os = "windows")': True,
            'cfg(target_os = "macos")': False,
            'cfg(unix)': False, 'cfg(windows)': True,
            'cfg(all(target_os = "macos", unknown))': False,
            'cfg(any(target_os = "macos", unknown))': None,
            'cfg(not(unknown))': None,
            'cfg(target_feature = "sse2")': None,
            'cfg(feature = "custom")': None,
            'cfg(not(windows))': False,
            'cfg(all(windows, target_arch = "x86_64",))': True,
            'cfg(unknown_function(windows))': None,
            'cfg(target_os = "macos" broken)': None,
            'aarch64-apple-darwin': False,
            'x86_64-pc-windows-msvc': True,
        }
        for expression, expected in cases.items():
            with self.subTest(expression=expression):
                self.assertIs(scope.target_matches(expression, context), expected)

    def test_unknown_cfg_dependency_metadata_and_features_stay_strict(self):
        self.app.write_text(self.app.read_text() + '\n[target.\'cfg(custom_runtime)\'.dependencies]\ncustom = {version = "1", optional = true}\n')
        identity = self.identity()
        canonical = scope.canonical_identity(identity, self.root)
        self.assertEqual(canonical["configuration"], identity["configuration"])

    def test_cargo_config_target_wrapper_and_cfg_overrides_disable_pruning(self):
        self.add_mac_dependency()
        mutations = (
            lambda x: x["configuration"]["cargo_configs"]["workspace"].update({"config.toml": "digest"}),
            lambda x: x["environment"].update(CARGO_BUILD_TARGET="aarch64-apple-darwin"),
            lambda x: x["environment"].update(RUSTC_WRAPPER="custom-wrapper"),
            lambda x: x["environment"].update(RUSTFLAGS="--cfg windows"),
            lambda x: x["environment"].update(CARGO_ENCODED_RUSTFLAGS="--cfg\x1fwindows"),
            lambda x: x["environment"].update(CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="--target=other"),
        )
        for mutate in mutations:
            identity = self.identity()
            mutate(identity)
            self.assertEqual(scope.canonical_identity(identity, self.root)["configuration"], identity["configuration"])

    def test_legacy_or_unknown_workspace_resolver_disables_pruning(self):
        self.add_mac_dependency()
        for resolver in (None, "1", "future"):
            with self.subTest(resolver=resolver):
                identity = self.identity()
                identity["configuration"]["manifests"]["apps/maple-agent/Cargo.toml"]["resolver"] = resolver
                self.assertEqual(scope.canonical_identity(identity, self.root)["configuration"], identity["configuration"])

    def test_unknown_native_triples_do_not_infer_architecture_or_target_os(self):
        self.add_mac_dependency()
        for system, host in (
            ("Windows", "i686-pc-windows-msvc"),
            ("Linux", "armv7-unknown-linux-gnueabihf"),
            ("Linux", "aarch64-linux-android"),
        ):
            with self.subTest(host=host):
                identity = self.identity(system)
                identity["rustc"]["version"] = f"rustc 1.98.0\nhost: {host}"
                self.assertIsNone(scope.native_target(identity))
                self.assertEqual(scope.canonical_identity(identity, self.root)["configuration"], identity["configuration"])

    def test_abi_toolchain_flags_profiles_and_configuration_are_not_weakened(self):
        self.add_mac_dependency()
        identity = self.identity()
        before = self.key(identity)
        for section, key, value in (
            ("native", "sdk", "changed"), ("rustc", "path", "/different/rustc"),
            ("environment", "RUSTFLAGS", "-D warnings -C opt-level=2"),
            ("environment", "CARGO_PROFILE_DEV_DEBUG", "1"),
        ):
            with self.subTest(section=section, key=key):
                changed = copy.deepcopy(identity)
                changed[section][key] = value
                self.assertNotEqual(self.key(changed), before)
        changed = copy.deepcopy(identity)
        changed["configuration"]["files"]["guard"] = "changed"
        self.assertNotEqual(self.key(changed), before)
        self.app.write_text(self.app.read_text() + '\n[profile.dev]\nopt-level = 2\n')
        self.assertNotEqual(self.key(self.identity()), before)

    def test_flat_migration_document_preserves_exact_legacy_and_canonical_hashes(self):
        self.add_mac_dependency()
        legacy = self.identity()
        with patch.object(agent_cache, "build_identity", return_value=legacy) as build:
            document = scope.build_document("ci", self.root, self.root / "provider")
        build.assert_called_once_with("ci", self.root, self.root / "provider")
        self.assertEqual(document["legacy_identity"], legacy)
        self.assertEqual(document["legacy_compatibility"], agent_cache.digest(legacy))
        canonical = {key: value for key, value in document.items() if key not in {"compatibility", "legacy_compatibility", "legacy_identity"}}
        self.assertEqual(document["compatibility"], agent_cache.digest(canonical))
        self.assertEqual(canonical["scope_schema"], 1)
        self.assertIn("rustc", document)
        self.assertEqual(legacy, self.identity(), "canonicalization mutated the legacy migration identity")

    def test_cli_preserves_profile_directory_and_publishes_both_hashes(self):
        self.add_mac_dependency()
        with patch.object(agent_cache, "build_identity", return_value=self.identity()):
            document = scope.build_document("provider", self.root)
        destination = self.root / "metrics/environment.json"
        outputs = self.root / "outputs"
        provider = self.root / "copied-provider"
        arguments = ["agent_cache_scope.py", "--profile", "provider", "--output", str(destination),
                     "--cargo-config-dir", str(provider)]
        with patch.object(scope.sys, "argv", arguments), patch.object(scope, "build_document", return_value=document) as build, \
                patch.dict(os.environ, {"GITHUB_OUTPUT": str(outputs)}), patch("sys.stdout", new_callable=io.StringIO):
            scope.main()
        build.assert_called_once_with("provider", cargo_config_dir=provider)
        self.assertEqual(json.loads(destination.read_text()), document)
        self.assertEqual(outputs.read_text().splitlines(), [
            f"compatibility={document['compatibility']}",
            f"legacy_compatibility={document['legacy_compatibility']}",
        ])

    def test_windows_manifest_names_and_unsafe_paths(self):
        self.add_mac_dependency()
        identity = self.identity()
        manifests = identity["configuration"]["manifests"]
        manifests[APP.replace("/", "\\")] = manifests.pop(APP)
        scope.canonical_identity(identity, self.root)
        for relative in ("../outside/Cargo.toml", "/outside/Cargo.toml", "C:\\outside\\Cargo.toml"):
            with self.subTest(relative=relative), self.assertRaises(ValueError):
                scope.manifest_path(self.root, relative)


if __name__ == "__main__":
    unittest.main()
