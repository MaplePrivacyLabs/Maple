import hashlib
import importlib.util
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
import zipfile


SCRIPT = Path(__file__).resolve().parents[1] / "package-archive.py"
spec = importlib.util.spec_from_file_location("package_archive", SCRIPT)
archive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(archive)


class PackageTests(unittest.TestCase):
    def test_archive_includes_binary_runtime_worker_licenses_and_checksum(self):
        for windows in (False, True):
            with self.subTest(windows=windows), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                binary = root / ("maple-gpui.exe" if windows else "maple-gpui")
                binary.write_bytes(b"maple executable")
                runtime = root / "python"
                runtime.mkdir()
                executable = "python.exe" if windows else "bin/python3.13"
                (runtime / executable).parent.mkdir(exist_ok=True)
                (runtime / executable).write_bytes(b"python executable")
                (runtime / "worker.py").write_text("worker")
                (runtime / "LICENSE.txt").write_text("python license")
                (runtime / "runtime.json").write_text(json.dumps({"distribution": "pbs-fixture", "executable": executable, "worker": "worker.py"}))
                result = archive.package(binary, runtime, "Maple test 日本語", root / "dist")
                if windows:
                    with zipfile.ZipFile(result) as package:
                        names = package.namelist()
                else:
                    with tarfile.open(result) as package:
                        names = package.getnames()
                for expected in [binary.name, "LICENSE", "runtime/python/runtime.json", "runtime/python/worker.py", "runtime/python/LICENSE.txt", f"runtime/python/{executable}"]:
                    self.assertIn(f"Maple test 日本語/{expected}", names)
                checksum = hashlib.sha256(result.read_bytes()).hexdigest()
                self.assertEqual(result.with_name(result.name + ".sha256").read_text(), f"{checksum}  {result.name}\n")


if __name__ == "__main__":
    unittest.main()
