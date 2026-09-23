#!/usr/bin/env python3
"""Add a local StoreKit Run scheme to the existing generated Maple project."""

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import xml.etree.ElementTree as ET


def parse_project(source):
    result = subprocess.run(["/usr/bin/plutil", "-convert", "json", "-o", "-", "--", "-"],
                            input=source, text=True, capture_output=True, timeout=5, check=True)
    return json.loads(result.stdout)


def register_fixture_reference(source, project, configuration):
    """Insert only a navigator reference; preserve other project bytes and objects."""
    parsed = parse_project(source)
    expected = copy.deepcopy(parsed)
    objects = expected["objects"]
    owner = objects[expected["rootObject"]]
    if owner.get("isa") != "PBXProject" or owner.get("projectDirPath", ""):
        raise ValueError("Unexpected project root; refusing to guess StoreKit reference paths")
    group_id = owner["mainGroup"]
    group = objects[group_id]
    if group.get("isa") != "PBXGroup" or not isinstance(group.get("children"), list):
        raise ValueError("Expected main PBXGroup children")
    target = configuration.resolve()
    relative = os.path.relpath(target, project.parent.resolve())
    matches = []
    for identifier, value in objects.items():
        if value.get("isa") != "PBXFileReference" or not value.get("path"):
            continue
        base = None
        if value.get("sourceTree") == "SOURCE_ROOT":
            base = project.parent
        elif (value.get("sourceTree") == "<group>" and identifier in group["children"]
              and group.get("sourceTree", "<group>") in ("<group>", "SOURCE_ROOT")):
            base = project.parent / group.get("path", "")
        if base is not None and (base / value["path"]).resolve() == target:
            matches.append(identifier)
    if len(matches) > 1:
        raise ValueError("Multiple existing StoreKit fixture references; resolve them explicitly")
    identifier = matches[0] if matches else hashlib.sha256(
        ("maple-storekit-navigator:" + relative).encode()).hexdigest()[:24].upper()
    if any(value.get("isa") == "PBXBuildFile" and value.get("fileRef") == identifier
           for value in objects.values()):
        raise ValueError("StoreKit fixture already has target membership; refusing to change build phases")
    if not matches:
        if identifier in objects:
            raise ValueError("StoreKit navigator reference ID collision")
        reference = {"isa": "PBXFileReference", "lastKnownFileType": "text",
                     "name": configuration.name, "path": relative, "sourceTree": "SOURCE_ROOT"}
        marker = "/* End PBXFileReference section */"
        if source.count(marker) != 1:
            raise ValueError("Expected one PBXFileReference section")
        # JSON quoting is also valid for these OpenStep strings. No shell use.
        fields = "; ".join(f"{key} = {json.dumps(value)}" for key, value in reference.items())
        entry = f"\t\t{identifier} /* Maple StoreKit fixture */ = {{{fields}; }};\n"
        source = source.replace(marker, entry + marker, 1)
        objects[identifier] = reference
    if identifier not in group["children"]:
        pattern = re.compile(r"(?ms)^([ \t]*)" + re.escape(group_id)
                             + r"(?: /\*[^\n]*\*/)? = \{(?P<body>.*?)^\1\};")
        groups = list(pattern.finditer(source))
        if len(groups) != 1:
            raise ValueError("Cannot locate unique main PBXGroup without rewriting the project")
        block = groups[0]
        children = list(re.finditer(r"(?m)^(?P<indent>[ \t]*)children = \([ \t]*$", block.group("body")))
        if len(children) != 1:
            raise ValueError("Cannot locate main PBXGroup children without rewriting the project")
        position = block.start("body") + children[0].end()
        entry = f"\n{children[0].group('indent')}\t{identifier} /* Maple StoreKit fixture */,"
        source = source[:position] + entry + source[position:]
        group["children"].insert(0, identifier)
    if parse_project(source) != expected:
        raise ValueError("StoreKit registration changed unexpected project objects")
    return source


def write_project_if_unchanged(path, original, updated):
    if original == updated:
        return
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, prefix=".storekit-", delete=False) as output:
            temporary = Path(output.name)
            output.write(updated)
        os.chmod(temporary, stat.S_IMODE(path.stat().st_mode))
        if path.read_text() != original:
            raise ValueError("Xcode project changed while preparing StoreKit; retry after saving it")
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def configure_scheme(tree, project, configuration, *, debugger=False):
    launch = tree.getroot().find("LaunchAction")
    if launch is None:
        raise ValueError("The generated maple_iOS scheme has no LaunchAction")
    # On this host LLDB symbol loading suspended Maple before its WebView started.
    # Keep debugger attachment opt-in for Run, without changing TestAction.
    launch.set("debugExecutable", "YES" if debugger else "NO")
    launch.set("selectedDebuggerIdentifier", "Xcode.DebuggerFoundation.Debugger.LLDB" if debugger else "")
    launch.set("selectedLauncherIdentifier", "Xcode.DebuggerFoundation.Launcher.LLDB" if debugger
               else "Xcode.IDEFoundation.Launcher.PosixSpawn")
    for prior in launch.findall("StoreKitConfigurationFileReference"):
        launch.remove(prior)
    # Xcode resolves this relative to the implicit workspace, not the .xcscheme.
    identifier = os.path.relpath(configuration, project / "project.xcworkspace")
    ET.SubElement(launch, "StoreKitConfigurationFileReference", {"identifier": identifier})
    return tree


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--debugger", action="store_true", help="Attach LLDB to Run (disabled by default to avoid the observed symbol-loading stall)")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[3]
    tauri = root / "apps/maple-research/frontend/src-tauri"
    project = tauri / "gen/apple/maple.xcodeproj"
    schemes = project / "xcshareddata/xcschemes"
    configuration = tauri / "tests/storekit/Maple.storekit"
    if not configuration.is_file():
        raise SystemExit("The local Maple.storekit fixture is missing")
    tree = configure_scheme(ET.parse(schemes / "maple_iOS.xcscheme"), project,
                            configuration, debugger=args.debugger)
    project_file = project / "project.pbxproj"
    original = project_file.read_text()
    updated = register_fixture_reference(original, project, configuration)
    write_project_if_unchanged(project_file, original, updated)
    ET.indent(tree, space="   ")
    output = schemes / "MapleStoreKitExperiment.xcscheme"
    tree.write(output, encoding="UTF-8", xml_declaration=True)
    print(output)
    print(f"Run debugger: {'LLDB enabled' if args.debugger else 'disabled (PosixSpawn)'}; TestAction unchanged.")
    print("Open maple.xcodeproj directly; select MapleStoreKitExperiment and the simulator.")
    print("Use Product > Perform Action > Run Without Building after the simulator build.")


if __name__ == "__main__":
    main()
