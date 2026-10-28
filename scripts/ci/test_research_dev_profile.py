import copy
import importlib.util
import json
from pathlib import Path
import plistlib
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("research_dev_profile", Path(__file__).with_name("research-dev-profile.py"))
profile_module = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(profile_module)


class ResearchDevProfileTests(unittest.TestCase):
    def setUp(self):
        self.profile = json.loads(profile_module.PROFILE.read_text())
        self.overlay = json.loads((profile_module.TAURI / "tauri.desktop-dev.conf.json").read_text())

    def test_no_production_identity_updater_or_storage(self):
        profile_module.validate_config(self.profile, self.overlay)
        mutations = [
            lambda o: o.update(identifier="cloud.opensecret.maple"),
            lambda o: o["plugins"]["deep-link"]["desktop"].update(schemes=["cloud.opensecret.maple"]),
            lambda o: o["plugins"]["updater"].update(endpoints=["https://updates.trymaple.ai/latest.json"]),
            lambda o: o["bundle"].update(createUpdaterArtifacts=True),
            lambda o: o["app"]["security"].update(capabilities=["default"]),
        ]
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                invalid = copy.deepcopy(self.overlay)
                mutate(invalid)
                with self.assertRaises(ValueError):
                    profile_module.validate_config(self.profile, invalid)

    def test_checks_actual_package_identity_and_scheme(self):
        with tempfile.TemporaryDirectory() as directory:
            bundle = Path(directory)
            (bundle / "Contents/MacOS").mkdir(parents=True)
            (bundle / "Contents/MacOS/maple").write_bytes(b"fixture executable")
            info = {"CFBundleIdentifier": self.profile["identifier"], "CFBundleName": self.profile["productName"],
                    "CFBundleExecutable": "maple", "CFBundleURLTypes": [{"CFBundleURLSchemes": [self.profile["scheme"]]}]}
            path = bundle / "Contents/Info.plist"
            path.write_bytes(plistlib.dumps(info))
            self.assertEqual(profile_module.verify_bundle(bundle, self.profile)["scheme"], "cloud.opensecret.maple.dev")
            info["CFBundleURLTypes"][0]["CFBundleURLSchemes"] = ["cloud.opensecret.maple"]
            path.write_bytes(plistlib.dumps(info))
            with self.assertRaises(ValueError):
                profile_module.verify_bundle(bundle, self.profile)

    def test_linux_configuration_is_isolated_without_changing_production(self):
        base = json.loads((profile_module.TAURI / "tauri.conf.json").read_text())
        original = copy.deepcopy(base)
        config = profile_module.linux_config(self.profile, base, self.overlay, "fixture/libonnxruntime.so")
        self.assertEqual(base, original)
        self.assertEqual(config["mainBinaryName"], "maple-research-dev")
        self.assertEqual(config["identifier"], self.profile["identifier"])
        self.assertEqual(config["bundle"]["targets"], ["deb"])
        self.assertEqual(config["bundle"]["linux"]["deb"]["files"],
                         {"/usr/lib/maple-research-dev/libonnxruntime.so": "fixture/libonnxruntime.so"})
        self.assertEqual(config["bundle"]["linux"]["rpm"]["depends"], ["libwebkit2gtk-4.1.so.0()(64bit)"])
        self.assertIn("linux", config["app"]["security"]["capabilities"][0]["platforms"])
        profile_module.validate_config(self.profile, config)

    def test_dev_requirements_do_not_depend_on_production_rpm_policy(self):
        for production_requires in (None, ["webkit2gtk4.1"], ["future-production-only-requirement"]):
            with self.subTest(production_requires=production_requires):
                base = json.loads((profile_module.TAURI / "tauri.conf.json").read_text())
                if production_requires is None:
                    base["bundle"]["linux"].pop("rpm", None)
                else:
                    base["bundle"]["linux"]["rpm"] = {"depends": production_requires}
                original = copy.deepcopy(base)
                config = profile_module.linux_config(self.profile, base, self.overlay, "fixture/libonnxruntime.so")
                self.assertEqual(base, original)
                self.assertEqual(config["bundle"]["linux"]["rpm"]["depends"], self.profile["linux"]["runtimeDependencies"])

    def test_dev_rpm_verifies_its_own_runtime_capability(self):
        capability = "libwebkit2gtk-4.1.so.0()(64bit)"
        for package_name, requires, valid in (
            ("maple-research-dev", capability + "\n", True),
            ("maple-research-dev", "webkit2gtk4.1\n", False),
            ("maple-research-dev", "", False),
            ("maple", capability + "\n", False),
        ):
            with self.subTest(package_name=package_name, requires=requires):
                with patch.object(profile_module.subprocess, "check_output", side_effect=[package_name, requires]):
                    if valid:
                        self.assertEqual(profile_module.verify_linux_rpm_requirements(Path("dev.rpm"), self.profile),
                                         {"runtime_requirements": [capability]})
                    else:
                        with self.assertRaises(ValueError):
                            profile_module.verify_linux_rpm_requirements(Path("dev.rpm"), self.profile)

    def linux_payload(self, root):
        files = {
            "usr/bin/maple-research-dev": b"fixture executable",
            "usr/lib/maple-research-dev/libonnxruntime.so": b"fixture library",
            "usr/share/icons/hicolor/128x128/apps/maple-research-dev.png": b"fixture icon",
            "usr/share/applications/maple-research-dev.desktop": b"""[Desktop Entry]
Name=Maple Research Dev
Exec=maple-research-dev %U
Icon=maple-research-dev
MimeType=x-scheme-handler/cloud.opensecret.maple.dev;
""",
        }
        for name, content in files.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        (root / "usr/bin/maple-research-dev").chmod(0o755)

    def test_linux_payload_checks_installed_names_and_rejects_collisions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.linux_payload(root)
            self.assertEqual(profile_module.verify_linux_payload(root, self.profile)["package_name"], "maple-research-dev")
            for collision in ("usr/bin/maple", "usr/lib/maple/libonnxruntime.so", "usr/share/applications/maple.desktop"):
                with self.subTest(collision=collision):
                    path = root / collision
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(b"production collision")
                    with self.assertRaisesRegex(ValueError, "production-owned"):
                        profile_module.verify_linux_payload(root, self.profile)
                    path.unlink()

    def test_linux_staging_normalizes_tauri_launcher_and_inherited_library(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.linux_payload(root)
            desktop = root / "usr/share/applications/maple-research-dev.desktop"
            desktop.write_text(desktop.read_text().replace("Exec=maple-research-dev %U", "Exec=maple-research-dev"))
            desktop.rename(desktop.with_name("Maple Research Dev.desktop"))
            inherited = root / "usr/lib/maple/libonnxruntime.so"
            inherited.parent.mkdir(parents=True)
            inherited.write_bytes(b"inherited production mapping")
            evidence = profile_module.normalize_linux_payload(root, self.profile)
            self.assertEqual(evidence["package_name"], "maple-research-dev")
            self.assertFalse(inherited.exists())
            self.assertIn("Exec=maple-research-dev %u", desktop.read_text())

    def test_linux_payload_rejects_links_into_another_installation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.linux_payload(root)
            icon = root / "usr/share/icons/hicolor/128x128/apps/maple-research-dev.png"
            icon.unlink()
            icon.symlink_to("/usr/share/icons/maple.png")
            with self.assertRaisesRegex(ValueError, "another installation"):
                profile_module.verify_linux_payload(root, self.profile)

    def test_linux_payload_rejects_production_launcher_and_scheme(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.linux_payload(root)
            path = root / "usr/share/applications/maple-research-dev.desktop"
            original = path.read_text()
            for old, new in (("Exec=maple-research-dev", "Exec=maple"),
                             ("cloud.opensecret.maple.dev", "cloud.opensecret.maple"),
                             ("Icon=maple-research-dev", "Icon=maple")):
                with self.subTest(replacement=new):
                    path.write_text(original.replace(old, new))
                    with self.assertRaises(ValueError):
                        profile_module.verify_linux_payload(root, self.profile)

    def test_runtime_audit_rejects_host_dependencies_and_search_paths(self):
        system_loads = "maple:\n\t/usr/lib/libiconv.2.dylib (compatibility version 7.0.0, current version 7.0.0)\n"
        bundle_rpath = "Load command 0\n cmd LC_RPATH\n path @executable_path/../Frameworks (offset 12)\n"
        with patch.object(profile_module.subprocess, "check_output", side_effect=[system_loads, bundle_rpath]):
            profile_module.verify_macos_runtime_paths(Path("fixture/maple"))
        for host_path in ("/nix/store/fixture/lib/libiconv.2.dylib", "/Volumes/runner/lib/libiconv.2.dylib", "/Users/runner/lib/libiconv.2.dylib"):
            with self.subTest(host_path=host_path):
                loads = system_loads.replace("/usr/lib/libiconv.2.dylib", host_path)
                with patch.object(profile_module.subprocess, "check_output", side_effect=[loads, bundle_rpath]):
                    with self.assertRaisesRegex(ValueError, "build-host runtime path"):
                        profile_module.verify_macos_runtime_paths(Path("fixture/maple"))
                commands = bundle_rpath.replace("@executable_path/../Frameworks", str(Path(host_path).parent))
                with patch.object(profile_module.subprocess, "check_output", side_effect=[system_loads, commands]):
                    with self.assertRaisesRegex(ValueError, "build-host runtime path"):
                        profile_module.verify_macos_runtime_paths(Path("fixture/maple"))


if __name__ == "__main__":
    unittest.main()
