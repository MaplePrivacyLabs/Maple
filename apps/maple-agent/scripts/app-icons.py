#!/usr/bin/env python3
"""Rebuild the Work app icons in app/packaging from their Icon Composer sources.

app/packaging/icon/<profile>/MapleAgent.icon is the artwork; open it in Icon
Composer to change it. For macOS, Xcode's actool compiles it to the asset
catalog that macOS 26 draws in every icon style (default, dark, tinted,
clear), with flattened copies for earlier releases, and the .icns the bundle
names as its fallback. Windows and Linux get flat rasters drawn from the same
layers in their default appearance; their trays scale the .png. Packaging only
copies the files written here, so commit them with the sources.

actool stamps each build of the catalog, so a rebuild with unchanged artwork
still changes the .car files; commit them only with an artwork change.

Needs macOS with Xcode 26 or later, and rsvg-convert (Homebrew librsvg).
"""

import json
from pathlib import Path
import plistlib
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ElementTree

COMPONENT = Path(__file__).resolve().parent.parent
PACKAGING = COMPONENT / "app/packaging"
SOURCES = PACKAGING / "icon"
PROFILES = {"prod": "maple-agent", "dev": "maple-agent-dev"}  # output stem per release profile
# CFBundleIconName; release-info.py names the same icon for both profiles.
ICON_NAME = "MapleAgent"
# The bundle's LSMinimumSystemVersion, so the catalog carries fallbacks for it.
MINIMUM_MACOS = "15.0"
ICO_SIZES = (16, 24, 32, 48, 64, 256)
PNG_SIZE = 512
# Windows and Linux draw icons unmasked, so their tile carries the brand kit's
# rounded corners (29 of 128) and fills the frame, as in the kit, which keeps
# the mark as large as it can be at 16 to 32 px.
TILE_RADIUS = 29 / 128
CANVAS = 1024  # Icon Composer's canvas, in points
SVG = '<svg xmlns="http://www.w3.org/2000/svg" width="{0}" height="{0}" viewBox="0 0 {0} {0}">{1}</svg>\n'


def color(value):
    """Return (#rrggbb, alpha) for an Icon Composer sRGB colour string."""
    match = re.fullmatch(r"srgb:([0-9.]+),([0-9.]+),([0-9.]+),([0-9.]+)", value)
    if not match:
        raise ValueError(f"only srgb colours are supported, not {value!r}")
    red, green, blue, alpha = (float(component) for component in match.groups())
    return "#" + "".join(f"{round(channel * 255):02X}" for channel in (red, green, blue)), alpha


def base_value(entry, key):
    """The default-appearance value of `key` or of `key`-specializations."""
    if key in entry:
        return entry[key]
    for specialization in entry.get(f"{key}-specializations", ()):
        if not {"appearance", "idiom", "localization"} & specialization.keys():
            return specialization["value"]
    return None


def solid(fill):
    if not isinstance(fill, dict) or set(fill) != {"solid"}:
        raise ValueError(f"only solid fills are supported, not {fill!r}")
    return color(fill["solid"])


def layer_paths(svg_path):
    """Path data and fill rules of a single-colour SVG layer, and its viewBox.

    Icon Composer places a layer at its own size, centred, so each layer is
    drawn on the full canvas and placed with the group's position instead.
    """
    root = ElementTree.parse(svg_path).getroot()
    if root.get("width") != str(CANVAS) or root.get("height") != str(CANVAS) or not root.get("viewBox"):
        raise ValueError(f"{svg_path} must be {CANVAS}x{CANVAS} with a viewBox")
    # Icon Composer draws any SVG; the flat icons read only plain paths, so refuse the rest.
    unsupported = [child.tag.rpartition("}")[2] for child in root
                   if child.tag != "{http://www.w3.org/2000/svg}path" or child.get("transform") or len(child)]
    if root.get("transform"):
        unsupported.insert(0, "svg")
    if unsupported:
        raise ValueError(f"{svg_path} may hold only <path> elements without transforms, not {unsupported}")
    if any(not child.get("d") for child in root):
        raise ValueError(f"{svg_path} has a <path> without path data")
    view_box = [float(number) for number in root.get("viewBox", "").split()]
    paths = [(element.get("d"), element.get("fill-rule", "nonzero")) for element in root]
    if not paths:
        raise ValueError(f"{svg_path} has no paths")
    return view_box, paths


# A placement (sx, sy, tx, ty) maps a point (x, y) to (sx * x + tx, sy * y + ty).
IDENTITY = (1.0, 1.0, 0.0, 0.0)


def then(outer, inner):
    """The placement that applies `inner`, then `outer`."""
    return (outer[0] * inner[0], outer[1] * inner[1], outer[0] * inner[2] + outer[2], outer[1] * inner[3] + outer[3])


def position(entry):
    """Icon Composer scales a group or layer about the canvas centre, then moves it in points."""
    value = base_value(entry, "position")
    if not value:
        return IDENTITY
    scale = value["scale"]
    dx, dy = value["translation-in-points"]
    shift = CANVAS / 2 * (1 - scale)
    return (scale, scale, shift + dx, shift + dy)


def layers(icon):
    """Yield (placement, paths, colour, alpha) back to front, in the default appearance."""
    document = json.loads((icon / "icon.json").read_text(encoding="utf-8"))
    for group in reversed(document["groups"]):
        if base_value(group, "hidden"):
            continue
        group_opacity = base_value(group, "opacity")
        for layer in reversed(group["layers"]):
            if base_value(layer, "hidden"):
                continue
            (vx, vy, vw, vh), paths = layer_paths(icon / "Assets" / layer["image-name"])
            fit = (CANVAS / vw, CANVAS / vh, -vx * CANVAS / vw, -vy * CANVAS / vh)
            fill = base_value(layer, "fill")
            if not fill:
                # Without one Icon Composer keeps the artwork's colours, which the flat icons can't.
                raise ValueError(f"layer {layer['name']!r} needs a solid fill for the Windows and Linux icons")
            hex_color, alpha = solid(fill)
            opacity = base_value(layer, "opacity")
            yield (then(position(group), then(position(layer), fit)), paths, hex_color,
                   alpha * (1.0 if opacity is None else opacity) * (1.0 if group_opacity is None else group_opacity))


def background(icon):
    document = json.loads((icon / "icon.json").read_text(encoding="utf-8"))
    return solid(base_value(document, "fill"))


def path_elements(items):
    out = []
    for (sx, sy, tx, ty), paths, hex_color, opacity in items:
        for data, rule in paths:
            out.append(f'<path transform="matrix({sx:g} 0 0 {sy:g} {tx:g} {ty:g})" fill="{hex_color}" '
                       f'fill-opacity="{opacity:g}" fill-rule="{rule}" d="{data}"/>')
    return "".join(out)


def flat_svg(icon):
    """The default appearance on the kit's rounded tile, for Windows and Linux."""
    tile, tile_alpha = background(icon)
    return SVG.format(CANVAS, f'<defs><clipPath id="tile"><rect width="{CANVAS}" height="{CANVAS}" '
                              f'rx="{CANVAS * TILE_RADIUS:g}"/></clipPath></defs><g clip-path="url(#tile)">'
                              f'<rect width="{CANVAS}" height="{CANVAS}" fill="{tile}" fill-opacity="{tile_alpha:g}"/>'
                              f'{path_elements(layers(icon))}</g>')


def rasterize(svg, size, work):
    source = work / "artwork.svg"
    source.write_text(svg, encoding="utf-8")
    output = work / f"artwork-{size}.png"
    subprocess.run(["rsvg-convert", "--width", str(size), "--height", str(size), "--output", str(output), str(source)], check=True)
    return output.read_bytes()


def ico(frames):
    """An ICO with one PNG frame per size (Windows Vista and later)."""
    if not all(1 <= size <= 256 for size, _ in frames):
        raise ValueError("ICO frames must be 1 to 256 px")
    header = struct.pack("<HHH", 0, 1, len(frames))
    offset = len(header) + 16 * len(frames)
    entries, blobs = [], []
    for size, blob in frames:
        entries.append(struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(blob), offset))
        blobs.append(blob)
        offset += len(blob)
    return header + b"".join(entries) + b"".join(blobs)


def compile_macos(icon, work):
    """actool's asset catalog and .icns for one profile."""
    output = work / "compiled"
    output.mkdir()
    # actool resolves relative paths against its own working directory.
    result = subprocess.run([
        "xcrun", "actool", str(icon.resolve()), "--compile", str(output.resolve()),
        "--platform", "macosx", "--target-device", "mac", "--minimum-deployment-target", MINIMUM_MACOS,
        "--app-icon", ICON_NAME, "--output-partial-info-plist", str((work / "partial.plist").resolve()),
        "--errors", "--warnings", "--output-format", "human-readable-text",
    ], capture_output=True, text=True)
    # On success actool lists the files it wrote; show only problems.
    report = result.stdout + result.stderr
    if result.returncode or re.search(r"\b(warning|error)\b", report, re.IGNORECASE):
        sys.stderr.write(report)
    result.check_returncode()
    partial = plistlib.loads((work / "partial.plist").read_bytes())
    if partial.get("CFBundleIconName") != ICON_NAME:
        raise RuntimeError(f"actool did not compile an icon named {ICON_NAME}: {partial}")
    return (output / "Assets.car").read_bytes(), (output / f"{ICON_NAME}.icns").read_bytes()


def main():
    if sys.platform != "darwin" or not shutil.which("xcrun") or not shutil.which("rsvg-convert"):
        sys.exit("app-icons.py needs macOS with Xcode 26 or later and rsvg-convert (brew install librsvg)")
    outputs = {}
    for profile, stem in PROFILES.items():
        icon = SOURCES / profile / f"{ICON_NAME}.icon"
        with tempfile.TemporaryDirectory(prefix=f"maple-icon-{profile}-") as temporary:
            work = Path(temporary)
            outputs[f"{stem}.car"], outputs[f"{stem}.icns"] = compile_macos(icon, work)
            flat = flat_svg(icon)
            outputs[f"{stem}.ico"] = ico([(size, rasterize(flat, size, work)) for size in ICO_SIZES])
            outputs[f"{stem}.png"] = rasterize(flat, PNG_SIZE, work)
    # Write once both profiles have built, so a failure leaves the committed files as they were.
    for name, data in outputs.items():
        (PACKAGING / name).write_bytes(data)
    print("wrote " + ", ".join(f"app/packaging/{name}" for name in outputs))


if __name__ == "__main__":
    main()
