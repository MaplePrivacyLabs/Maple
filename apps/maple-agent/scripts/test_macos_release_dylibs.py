#!/usr/bin/env python3
"""Hermetic Mach-O closure tests; native tools are mocked on every platform."""

import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location(
    "macos_release_dylibs", Path(__file__).with_name("macos-release-dylibs.py")
)
packaging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packaging)


def image(path, dependencies=(), rpaths=(), identifier=None, kinds=()):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({
        "dependencies": list(dependencies), "rpaths": list(rpaths),
        "identifier": identifier, "kinds": list(kinds),
    }, sort_keys=True))


class MachOTools:
    def __call__(self, tool, *args):
        path = Path(args[-1])
        state = json.loads(path.read_text())
        if tool == "otool":
            if args[0] == "-L":
                names = ([state["identifier"]] if state["identifier"] else []) + state["dependencies"]
                return str(path) + ":\n" + "".join(
                    f"\t{name} (compatibility version 1.0.0, current version 1.0.0)\n" for name in names
                )
            if args[0] == "-D":
                return f"{path}:\n" + (state["identifier"] + "\n" if state["identifier"] else "")
            if args[0] == "-l":
                return "".join(f"          cmd LC_RPATH\n         path {value} (offset 12)\n" for value in state["rpaths"])
        if tool == "install_name_tool":
            if args[0] == "-change":
                state["dependencies"] = [args[2] if value == args[1] else value for value in state["dependencies"]]
            elif args[0] == "-id":
                state["identifier"] = args[1]
            elif args[0] == "-delete_rpath":
                state["rpaths"].remove(args[1])
            elif args[0] == "-add_rpath":
                state["rpaths"].append(args[1])
            else:
                raise AssertionError(args)
            path.write_text(json.dumps(state, sort_keys=True))
            return ""
        raise AssertionError((tool, args))


class DylibTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "original/maple-agent"
        self.app = self.root / "staged/Maple Agent.app"
        self.binary = self.app / "Contents/MacOS/maple-agent"
        self.frameworks = self.app / "Contents/Frameworks"
        self.frameworks.mkdir(parents=True)
        self.binary.parent.mkdir(parents=True)
        self.iconv = self.root / "nix-runtime/lib/libiconv.2.dylib"
        self.charset = self.root / "nix-runtime/lib/libcharset.1.dylib"
        image(self.charset, ("/usr/lib/libSystem.B.dylib",), identifier=str(self.charset))
        image(self.iconv, (str(self.charset), "/usr/lib/libSystem.B.dylib"), identifier=str(self.iconv), kinds=("reexport", "load"))
        image(self.source, (str(self.iconv), "/System/Library/Frameworks/AppKit.framework/AppKit", "@rpath/libswiftCompatibility.dylib"), ("/usr/lib/swift", "@executable_path/../Frameworks"))
        self.tools = mock.patch.object(packaging, "native", MachOTools())
        self.tools.start()
        self.addCleanup(self.tools.stop)

    def relocate(self):
        shutil.copy2(self.source, self.binary)
        return packaging.Relocator(self.source, self.app).relocate()

    def test_recursive_iconv_reexport_and_source_immutability(self):
        originals = {path: packaging.digest(path) for path in (self.source, self.iconv, self.charset)}
        result = self.relocate()
        self.assertEqual([item["name"] for item in result["libraries"]], ["libcharset.1.dylib", "libiconv.2.dylib"])
        self.assertEqual(packaging.inspect(self.binary)[0][0], "@rpath/libiconv.2.dylib")
        state = json.loads((self.frameworks / self.iconv.name).read_text())
        self.assertEqual(state["dependencies"][0], "@rpath/libcharset.1.dylib")
        self.assertEqual(state["kinds"][0], "reexport")
        self.assertEqual(state["identifier"], "@rpath/libiconv.2.dylib")
        self.assertEqual(state["rpaths"], ["/usr/lib/swift", "@loader_path"])
        self.assertEqual(originals, {path: packaging.digest(path) for path in originals})
        self.assertNotIn(str(self.root), json.dumps(result))

    def test_original_loader_and_ancestor_rpath_context(self):
        plugin = self.root / "original/plugins/libplugin.dylib"
        nested = plugin.parent / "nested/libnested.dylib"
        image(nested, ("/usr/lib/libSystem.B.dylib",), identifier=str(nested))
        image(plugin, ("@rpath/libnested.dylib",), ("@loader_path/nested",), str(plugin))
        image(self.source, ("@rpath/libplugin.dylib",), ("@loader_path/plugins",))
        result = self.relocate()
        self.assertEqual(len(result["libraries"]), 2)
        self.assertEqual(packaging.inspect(self.frameworks / plugin.name)[0], ["@rpath/libnested.dylib"])

    def test_dylib_can_use_main_ancestor_rpath(self):
        plugin = self.root / "original/plugins/libplugin.dylib"
        nested = plugin.parent / "libnested.dylib"
        image(nested, identifier=str(nested))
        image(plugin, ("@rpath/libnested.dylib",), identifier=str(plugin))
        image(self.source, ("@rpath/libplugin.dylib",), ("@executable_path/plugins",))
        self.assertEqual(len(self.relocate()["libraries"]), 2)

    def test_aliases_and_cycles_copy_once(self):
        alias = self.iconv.parent / "libiconv-alias.dylib"
        alias.symlink_to(self.iconv.name)
        image(self.charset, ("@loader_path/libiconv.2.dylib",), identifier=str(self.charset))
        image(self.source, (str(self.iconv), str(alias)))
        result = self.relocate()
        self.assertEqual(len(result["libraries"]), 2)
        self.assertEqual(packaging.inspect(self.binary)[0], ["@rpath/libiconv.2.dylib"])

    def test_distinct_source_basename_collision_is_rejected(self):
        other = self.root / "different/lib/libiconv.2.dylib"
        image(other, identifier=str(other))
        image(self.source, (str(self.iconv), str(other)))
        with self.assertRaisesRegex(packaging.PackagingError, "share package basename"):
            self.relocate()

    def test_missing_library_is_rejected(self):
        self.charset.unlink()
        with self.assertRaisesRegex(packaging.PackagingError, "Cannot resolve dylib"):
            self.relocate()

    def test_missing_rpath_library_is_rejected(self):
        image(self.source, ("@rpath/libmissing.dylib",), ("@loader_path/plugins",))
        with self.assertRaisesRegex(packaging.PackagingError, "Cannot resolve dylib"):
            self.relocate()

    def test_relative_install_name_and_third_party_framework_are_rejected(self):
        image(self.source, ("librelative.dylib",))
        with self.assertRaisesRegex(packaging.PackagingError, "Unsupported runtime search path"):
            self.relocate()
        framework = self.root / "ThirdParty.framework/ThirdParty"
        image(framework, identifier=str(framework))
        image(self.source, (str(framework),))
        with self.assertRaisesRegex(packaging.PackagingError, "Unsupported non-system framework"):
            self.relocate()

    def test_source_alias_cannot_be_mutated_through_staged_hardlink(self):
        self.binary.hardlink_to(self.source)
        with self.assertRaisesRegex(packaging.PackagingError, "separate copy"):
            packaging.Relocator(self.source, self.app)

    def test_final_guard_rejects_build_host_rpaths(self):
        self.relocate()
        image(self.binary, ("@rpath/libiconv.2.dylib",), ("/nix/store/unexpected/lib",))
        # Create a new reader against the already-relocated package without
        # reinitializing the source-copy precondition.
        inspector = packaging.Relocator.__new__(packaging.Relocator)
        inspector.binary = self.binary
        inspector.frameworks = self.frameworks
        inspector.by_name = {}
        with self.assertRaisesRegex(packaging.PackagingError, "Unexpected staged runtime search paths"):
            inspector.audit()


if __name__ == "__main__":
    unittest.main()
