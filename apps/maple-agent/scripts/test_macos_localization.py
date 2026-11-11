"""Verify that both existing plist generators retain shipped native languages."""
import importlib.util
from pathlib import Path
import plistlib
import tempfile
import subprocess
import sys
import os
import unittest

COMPONENT = Path(__file__).resolve().parent.parent


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, COMPONENT / "scripts" / filename)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


class NativeLocalizationTests(unittest.TestCase):
    def setUp(self):
        self.base = plistlib.loads((COMPONENT / "app/macos/Info.plist").read_bytes())
        self.languages = sorted(path.name for path in (COMPONENT / "app/locales").iterdir() if path.is_dir())

    def assert_languages(self, result):
        self.assertEqual(sorted(result["CFBundleLocalizations"]), self.languages)
        self.assertEqual(result["CFBundleDevelopmentRegion"], "en")

    def assert_resources(self, plist_path, display_name):
        purpose_keys = {key for key in self.base if key.endswith("UsageDescription")}
        for locale in self.languages:
            content = plistlib.loads((plist_path.parent / "Resources" / f"{locale}.lproj/InfoPlist.strings").read_bytes())
            self.assertEqual(content["CFBundleDisplayName"], display_name)
            self.assertEqual(content["CFBundleName"], display_name)
            if locale != "en":
                expected = plistlib.loads((COMPONENT / f"app/macos/{locale}.lproj/InfoPlist.strings").read_bytes())
                self.assertEqual(set(expected), purpose_keys)
                for key in purpose_keys:
                    self.assertEqual(content[key], expected[key])
                    self.assertNotEqual(content[key], self.base[key])

    def test_debug_generator_preserves_language_metadata(self):
        debug = module("debug_plist", "macos-debug-plist.py")
        self.assert_languages(debug.configure(self.base, {}))
        with tempfile.TemporaryDirectory(prefix="maple-i18n-native-metadata-") as temporary:
            path = Path(temporary) / "Info.plist"
            path.write_bytes(plistlib.dumps(self.base))
            environment = {key: value for key, value in os.environ.items() if not key.startswith("MAPLE_")}
            subprocess.run([sys.executable, "-B", str(COMPONENT / "scripts/macos-debug-plist.py"), str(path)], check=True, env=environment)
            self.assert_resources(path, self.base["CFBundleName"])

    def test_release_generator_preserves_language_metadata(self):
        release = module("release_info", "release-info.py")
        with tempfile.TemporaryDirectory(prefix="maple-i18n-native-metadata-") as temporary:
            for profile in ("dev", "prod"):
                path = Path(temporary) / "Info.plist"
                release.write_plist({"profile": profile, "display_name": "Maple", "bundle_id": "test.maple", "version": "1.2.3"}, path, "1")
                self.assert_languages(plistlib.loads(path.read_bytes()))
                self.assert_resources(path, "Maple")


if __name__ == "__main__":
    unittest.main()
