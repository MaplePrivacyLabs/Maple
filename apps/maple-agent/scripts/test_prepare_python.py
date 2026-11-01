import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parent / "prepare-python.py"
spec = importlib.util.spec_from_file_location("prepare_python", SCRIPT)
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


class PreparationTests(unittest.TestCase):
    def archive(self, root, entries):
        path = root / "fixture.tar.gz"
        with tarfile.open(path, "w:gz") as archive:
            for name, value, link in entries:
                member = tarfile.TarInfo(name)
                if link:
                    member.type = tarfile.SYMTYPE
                    member.linkname = value
                    archive.addfile(member)
                else:
                    value = value.encode()
                    member.size = len(value)
                    archive.addfile(member, io.BytesIO(value))
        return path

    def test_archive_rejects_traversal_and_external_links(self):
        for entry in [
            ("python/../../escape", "bad", False),
            ("/absolute", "bad", False),
            ("python/bin/link", "../../../escape", True),
            ("python/bin/link", "/absolute", True),
            ("python/C:escape", "bad", False),
        ]:
            with self.subTest(entry=entry), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                archive = self.archive(root, [entry])
                with self.assertRaises(ValueError):
                    prepare.safe_extract(archive, root / "stage")

    def test_archive_preserves_internal_link_and_strips_one_root(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = self.archive(root, [
                ("python/bin/python3.14", "binary", False),
                ("python/bin/python3", "python3.14", True),
            ])
            staged = prepare.safe_extract(archive, root / "stage")
            self.assertEqual((staged / "bin/python3").read_text(), "binary")
            self.assertFalse((staged / "python").exists())

    def test_digest_mismatch_never_extracts_or_replaces_good_staging(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache = root / "cache"
            asset = {"sha256": "0" * 64, "url": "https://example.invalid/archive"}
            with patch.object(prepare.urllib.request, "urlopen", return_value=io.BytesIO(b"wrong bytes")):
                with self.assertRaisesRegex(ValueError, "SHA-256"):
                    prepare.download(asset, cache, False)
            self.assertEqual(list(cache.iterdir()), [])

    def test_verified_cache_and_stage_reused_corruption_repaired_offline(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = self.archive(root, [
                ("python/bin/python3.14", "binary", False),
                ("python/LICENSE.txt", "license", False),
                ("python/lib/module.py", "module", False),
            ])
            archive_hash = hashlib.sha256(archive.read_bytes()).hexdigest()
            cache = root / "cache"
            cache.mkdir()
            archive.rename(cache / f"{archive_hash}.tar.gz")
            worker = root / "worker.py"
            worker.write_text("worker")
            pins = root / "pins.json"
            pins.write_text(json.dumps({"implementation": "cpython", "version": "3.14.7", "release": "20260929", "targets": {"test": {"sha256": archive_hash, "executable": "bin/python3.14", "url": "unused"}}}))
            destination = root / "runtime"
            with patch.object(prepare, "PINS", pins), patch.object(prepare.urllib.request, "urlopen", side_effect=AssertionError("network forbidden")):
                prepare.prepare_pbs("test", destination, cache, worker, True)
                stamp = (destination / "runtime.json").stat().st_mtime_ns
                prepare.prepare_pbs("test", destination, cache, worker, True)
                self.assertEqual((destination / "runtime.json").stat().st_mtime_ns, stamp)
                (destination / "lib/module.py").write_text("corrupt")
                prepare.prepare_pbs("test", destination, cache, worker, True)
                self.assertEqual((destination / "lib/module.py").read_text(), "module")
                worker.write_text("updated worker")
                prepare.prepare_pbs("test", destination, cache, worker, True)
                self.assertEqual((destination / "worker.py").read_text(), "updated worker")

    def test_tk_and_idle_are_left_out_of_every_runtime(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = self.archive(root, [
                ("python/bin/python3.14", "binary", False),
                ("python/bin/idle3.14", "idle", False),
                ("python/bin/idle3", "idle3.14", True),
                ("python/LICENSE.txt", "license", False),
                ("python/lib/libtcl9.0.so", "tcl", False),
                ("python/lib/tcl9.0/init.tcl", "tcl", False),
                ("python/lib/tk9.0/tk.tcl", "tk", False),
                ("python/lib/python3.14/tkinter/__init__.py", "tkinter", False),
                ("python/lib/python3.14/idlelib/idle.py", "idle", False),
                ("python/lib/python3.14/lib-dynload/_tkinter.cpython-314-x86_64-linux-gnu.so", "ext", False),
                ("python/lib/python3.14/lib-dynload/_ssl.cpython-314-x86_64-linux-gnu.so", "ssl", False),
                ("python/lib/python3.14/json/__init__.py", "json", False),
            ])
            archive_hash = hashlib.sha256(archive.read_bytes()).hexdigest()
            cache = root / "cache"
            cache.mkdir()
            archive.rename(cache / f"{archive_hash}.tar.gz")
            worker = root / "worker.py"
            worker.write_text("worker")
            pins = root / "pins.json"
            pins.write_text(json.dumps({"implementation": "cpython", "version": "3.14.7", "release": "20260929", "targets": {"test": {"sha256": archive_hash, "executable": "bin/python3.14", "url": "unused"}}}))
            destination = root / "runtime"
            with patch.object(prepare, "PINS", pins), patch.object(prepare.urllib.request, "urlopen", side_effect=AssertionError("network forbidden")):
                prepare.prepare_pbs("test", destination, cache, worker, True)
            kept = sorted(path.relative_to(destination).as_posix() for path in destination.rglob("*")
                          if path.is_file() and not path.name.startswith("."))
            self.assertIn("lib/python3.14/lib-dynload/_ssl.cpython-314-x86_64-linux-gnu.so", kept)
            self.assertIn("lib/python3.14/json/__init__.py", kept)
            # The pinned upstream notice set stays complete, including Tcl's.
            for name in (name for name in kept if not name.startswith("licenses/")):
                self.assertNotRegex(name, r"tcl|tk9|tkinter|idle")
            self.assertFalse((destination / "bin/idle3").is_symlink())

    def test_failed_publication_restores_previous_tree(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination = root / "runtime"
            destination.mkdir()
            (destination / "valid").write_text("retained")
            with self.assertRaises(FileNotFoundError):
                prepare.replace_directory(root / "missing", destination)
            self.assertEqual((destination / "valid").read_text(), "retained")

    def test_nix_requires_explicit_manifest(self):
        with self.assertRaisesRegex(ValueError, "explicit"):
            prepare.verify_nix(None)


if __name__ == "__main__":
    unittest.main()
