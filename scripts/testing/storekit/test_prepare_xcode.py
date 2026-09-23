"""Pure scheme-generation checks; never read or write the live Xcode project."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET


spec = importlib.util.spec_from_file_location("prepare_xcode", Path(__file__).with_name("prepare-xcode.py"))
generator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(generator)


class PrepareSchemeTests(unittest.TestCase):
    def scheme(self):
        return ET.ElementTree(ET.fromstring('''<Scheme>
            <TestAction selectedDebuggerIdentifier="Test.Debugger"
                selectedLauncherIdentifier="Test.Launcher" debugExecutable="YES"/>
            <LaunchAction debugExecutable="YES" selectedDebuggerIdentifier="Old.Debugger"
                selectedLauncherIdentifier="Old.Launcher" buildConfiguration="debug">
                <StoreKitConfigurationFileReference identifier="stale.storekit"/>
                <BuildableProductRunnable runnableDebuggingMode="0"/>
            </LaunchAction>
        </Scheme>'''))

    def check_mode(self, debugger):
        tree = self.scheme()
        before_test = ET.tostring(tree.getroot().find("TestAction"))
        options = {} if debugger is None else {"debugger": debugger}
        generator.configure_scheme(tree, Path("/fixture/gen/apple/maple.xcodeproj"),
                                   Path("/fixture/tests/storekit/Maple.storekit"), **options)
        launch = tree.getroot().find("LaunchAction")
        self.assertEqual(ET.tostring(tree.getroot().find("TestAction")), before_test)
        self.assertEqual(launch.get("buildConfiguration"), "debug")
        self.assertIsNotNone(launch.find("BuildableProductRunnable"))
        references = launch.findall("StoreKitConfigurationFileReference")
        self.assertEqual([item.get("identifier") for item in references],
                         ["../../../../tests/storekit/Maple.storekit"])
        return launch

    def test_default_run_uses_posix_spawn_and_preserves_test_debugger(self):
        launch = self.check_mode(None)
        self.assertEqual(launch.get("debugExecutable"), "NO")
        self.assertEqual(launch.get("selectedDebuggerIdentifier"), "")
        self.assertEqual(launch.get("selectedLauncherIdentifier"), "Xcode.IDEFoundation.Launcher.PosixSpawn")

    def test_explicit_debugger_restores_lldb_only_for_run(self):
        launch = self.check_mode(True)
        self.assertEqual(launch.get("debugExecutable"), "YES")
        self.assertEqual(launch.get("selectedDebuggerIdentifier"), "Xcode.DebuggerFoundation.Debugger.LLDB")
        self.assertEqual(launch.get("selectedLauncherIdentifier"), "Xcode.DebuggerFoundation.Launcher.LLDB")

    def test_missing_launch_action_fails(self):
        with self.assertRaisesRegex(ValueError, "no LaunchAction"):
            generator.configure_scheme(ET.ElementTree(ET.Element("Scheme")),
                                       Path("/fixture/maple.xcodeproj"), Path("/fixture/Maple.storekit"))


@unittest.skipUnless(sys.platform == "darwin", "OpenStep project parsing requires macOS plutil")
class RegisterFixtureReferenceTests(unittest.TestCase):
    PROJECT = Path("/fixture/gen/apple/maple.xcodeproj")
    CONFIGURATION = Path("/fixture/tests/storekit/Maple.storekit")
    ROOT_ID = "111111111111111111111111"
    MAIN_ID = "222222222222222222222222"
    FILE_ID = "333333333333333333333333"
    FIXTURE_ID = "777777777777777777777777"

    def source(self, *, existing=False, membership=False):
        fixture_ref = ""
        fixture_child = ""
        membership_ref = ""
        membership_child = ""
        if existing:
            fixture_ref = (
                '\t\t777777777777777777777777 /* Existing Maple fixture */ = '
                '{isa = PBXFileReference; lastKnownFileType = text; '
                'name = "Existing Maple fixture"; '
                'path = "../../tests/storekit/../storekit/Maple.storekit"; '
                'sourceTree = "<group>"; };\n')
            fixture_child = '\t\t\t\t777777777777777777777777 /* Existing Maple fixture */,\n'
        if membership:
            membership_ref = (
                '\t\t888888888888888888888888 /* Maple.storekit in Resources */ = '
                '{isa = PBXBuildFile; fileRef = 777777777777777777777777 /* Existing Maple fixture */; };\n')
            membership_child = '\t\t\t\t888888888888888888888888 /* Maple.storekit in Resources */,\n'
        return '''// !$*UTF8*$!
{
    archiveVersion = 1;
    classes = {};
    objectVersion = 56;
    objects = {

/* Begin PBXBuildFile section */
        444444444444444444444444 /* App.swift in Sources */ = {isa = PBXBuildFile; fileRef = 333333333333333333333333 /* App.swift */; };
''' + membership_ref + '''/* End PBXBuildFile section */

/* Begin PBXFileReference section */
        333333333333333333333333 /* App.swift */ = {isa = PBXFileReference; lastKnownFileType = sourcecode.swift; path = App.swift; sourceTree = "<group>"; };
''' + fixture_ref + '''/* End PBXFileReference section */

/* Begin PBXGroup section */
        222222222222222222222222 = {
            isa = PBXGroup;
            children = (
                333333333333333333333333 /* App.swift */,
                AAAAAAAAAAAAAAAAAAAAAAAA /* Unrelated group */,
''' + fixture_child + '''            );
            sourceTree = "<group>";
        };
        AAAAAAAAAAAAAAAAAAAAAAAA /* Unrelated group */ = {
            isa = PBXGroup;
            children = ();
            path = Other;
            sourceTree = "<group>";
        };
/* End PBXGroup section */

/* Begin PBXNativeTarget section */
        555555555555555555555555 /* App */ = {
            isa = PBXNativeTarget;
            buildPhases = (
                666666666666666666666666 /* Sources */,
                999999999999999999999999 /* Resources */,
            );
            name = App;
            productType = "com.apple.product-type.application";
        };
/* End PBXNativeTarget section */

/* Begin PBXProject section */
        111111111111111111111111 /* Project object */ = {
            isa = PBXProject;
            mainGroup = 222222222222222222222222;
            targets = (555555555555555555555555 /* App */,);
        };
/* End PBXProject section */

/* Begin PBXResourcesBuildPhase section */
        999999999999999999999999 /* Resources */ = {
            isa = PBXResourcesBuildPhase;
            files = (
''' + membership_child + '''            );
        };
/* End PBXResourcesBuildPhase section */

/* Begin PBXSourcesBuildPhase section */
        666666666666666666666666 /* Sources */ = {
            isa = PBXSourcesBuildPhase;
            files = (444444444444444444444444 /* App.swift in Sources */,);
        };
/* End PBXSourcesBuildPhase section */
    };
    rootObject = 111111111111111111111111 /* Project object */;
}
'''

    def parsed(self, source):
        result = subprocess.run(
            ["/usr/bin/plutil", "-convert", "json", "-o", "-", "--", "-"],
            input=source, text=True, capture_output=True, check=True, timeout=5)
        return json.loads(result.stdout)

    def register(self, source):
        return generator.register_fixture_reference(source, self.PROJECT, self.CONFIGURATION)

    def test_first_registration_changes_only_main_group_and_adds_source_root_reference(self):
        source = self.source()
        result = self.register(source)
        before = self.parsed(source)
        after = self.parsed(result)
        added = set(after["objects"]) - set(before["objects"])
        self.assertEqual(len(added), 1)
        identifier = added.pop()
        self.assertRegex(identifier, r"^[0-9A-Fa-f]{24}$")
        reference = after["objects"].pop(identifier)
        self.assertEqual(reference["isa"], "PBXFileReference")
        self.assertEqual(reference["sourceTree"], "SOURCE_ROOT")
        self.assertEqual(reference["path"], "../../tests/storekit/Maple.storekit")
        children = after["objects"][self.MAIN_ID]["children"]
        self.assertEqual(children.count(identifier), 1)
        children.remove(identifier)
        self.assertEqual(after, before)
        # Preserve textual content in unrelated sections, including comments.
        for section in ("PBXBuildFile", "PBXNativeTarget", "PBXProject",
                        "PBXResourcesBuildPhase", "PBXSourcesBuildPhase"):
            start = f"/* Begin {section} section */"
            end = f"/* End {section} section */"
            original_section = source[source.index(start):source.index(end) + len(end)]
            self.assertIn(original_section, result)
        original_file = next(line for line in source.splitlines() if "lastKnownFileType = sourcecode.swift" in line)
        self.assertIn(original_file, result)

    def test_repeated_registration_is_byte_for_byte_unchanged(self):
        first = self.register(self.source())
        self.assertEqual(self.register(first), first)

    def test_existing_canonical_root_group_reference_is_reused_unchanged(self):
        source = self.source(existing=True)
        self.assertEqual(self.register(source), source)

    def test_fixture_with_target_membership_is_rejected(self):
        source = self.source(existing=True, membership=True)
        self.assertIn("888888888888888888888888",
                      self.parsed(source)["objects"]["999999999999999999999999"]["files"])
        with self.assertRaises(ValueError):
            self.register(source)

    def test_generated_identifier_collision_with_unrelated_file_is_rejected(self):
        source = self.source()
        first = self.parsed(self.register(source))
        identifier, = set(first["objects"]) - set(self.parsed(source)["objects"])
        collision = (
            f'        {identifier} /* Unrelated existing object */ = '
            '{isa = PBXFileReference; path = Other.asset; sourceTree = SOURCE_ROOT; };\n')
        source = source.replace("/* End PBXFileReference section */",
                                collision + "/* End PBXFileReference section */")
        with self.assertRaises(ValueError):
            self.register(source)

    def test_missing_main_group_children_is_rejected(self):
        source = self.source()
        start = source.index("            children = (")
        end = source.index("            );", start) + len("            );\n")
        source = source[:start] + source[end:]
        with self.assertRaises(ValueError):
            self.register(source)


class ProjectReplacementTests(unittest.TestCase):
    def test_replacement_preserves_mode_and_replaces_complete_bytes_without_leftover(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            project = directory / "project.pbxproj"
            original = "original project\n"
            updated = "updated project\nwith another line\n"
            project.write_text(original)
            project.chmod(0o640)
            real_replace = generator.os.replace

            def replace(source, destination):
                self.assertEqual(destination, project)
                self.assertEqual(project.read_text(), original)
                self.assertEqual(source.parent, directory)
                self.assertEqual(source.read_text(), updated)
                self.assertEqual(source.stat().st_mode & 0o777, 0o640)
                real_replace(source, destination)

            with patch.object(generator.os, "replace", side_effect=replace) as replacement:
                generator.write_project_if_unchanged(project, original, updated)
            replacement.assert_called_once()
            self.assertEqual(project.read_bytes(), updated.encode())
            self.assertEqual(project.stat().st_mode & 0o777, 0o640)
            self.assertEqual(list(directory.iterdir()), [project])

    def test_stale_original_preserves_current_file_and_cleans_temporary_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            project = directory / "project.pbxproj"
            current = b"newer Xcode save\n"
            project.write_bytes(current)
            project.chmod(0o640)
            with patch.object(generator.os, "replace") as replacement:
                with self.assertRaisesRegex(ValueError, "changed while preparing"):
                    generator.write_project_if_unchanged(project, "old project\n", "our update\n")
            replacement.assert_not_called()
            self.assertEqual(project.read_bytes(), current)
            self.assertEqual(project.stat().st_mode & 0o777, 0o640)
            self.assertEqual(list(directory.iterdir()), [project])


if __name__ == "__main__":
    unittest.main()
