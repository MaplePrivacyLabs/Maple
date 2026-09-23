"""Focused checks for the exact-bundle identity boundary used by run.py."""

from pathlib import Path
import tempfile
import unittest

from run import bundle_digest


class BundleIdentityTests(unittest.TestCase):
    def test_debug_dylib_and_resources_are_part_of_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            bundle = Path(temporary) / "Maple.app"
            bundle.mkdir()
            (bundle / "Maple").write_bytes(b"unchanged executable stub")
            dylib = bundle / "Maple.debug.dylib"
            dylib.write_bytes(b"implementation one")
            resource = bundle / "Info.plist"
            resource.write_bytes(b"resource one")
            original = bundle_digest(bundle)
            self.assertEqual(original, bundle_digest(bundle))
            dylib.write_bytes(b"implementation two")
            changed_code = bundle_digest(bundle)
            self.assertNotEqual(original, changed_code)
            resource.write_bytes(b"resource two")
            changed_resource = bundle_digest(bundle)
            self.assertNotEqual(changed_code, changed_resource)
            resource.rename(bundle / "Other.plist")
            self.assertNotEqual(changed_resource, bundle_digest(bundle))

    def test_symlinks_hash_the_reference_without_reading_outside_bundle(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            bundle = directory / "Maple.app"
            bundle.mkdir()
            external = directory / "outside"
            external.write_bytes(b"outside content")
            link = bundle / "reference"
            link.symlink_to("../outside")
            original = bundle_digest(bundle)
            external.write_bytes(b"unrelated outside change")
            self.assertEqual(original, bundle_digest(bundle))
            link.unlink()
            link.symlink_to("../different-outside")
            self.assertNotEqual(original, bundle_digest(bundle))


if __name__ == "__main__":
    unittest.main()
