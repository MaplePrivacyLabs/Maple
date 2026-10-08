"""Dependency closure over a selection.

Usage: python3 -I closure.py inv.json selection.txt [--npm] [--verbose]
selection.txt: one repo-relative path per line (comments with #).
Prints, for each selected file, imports whose symbols resolve to files
outside the selection (following export-* / export-from barrels).
"""
import json
import sys
from collections import defaultdict

inv = json.load(open(sys.argv[1]))
sel = set()
for line in open(sys.argv[2]):
    line = line.split("#", 1)[0].strip()
    if line:
        if line not in inv:
            print("NOT IN INVENTORY:", line)
        sel.add(line)
show_npm = "--npm" in sys.argv
verbose = "--verbose" in sys.argv


def reexports(mod):
    out = []
    for imp in inv[mod]["imports"]:
        if imp["kind"].startswith("export") and imp["res"][0] == "file":
            out.append(imp)
    return out


def imported_binding(mod, local):
    for imp in inv[mod]["imports"]:
        if imp["kind"].startswith("import") and imp["res"][0] == "file":
            for a, b, t in imp["names"]:
                if b == local:
                    return imp["res"][1], a
    return None


def resolve_symbol(mod, name, seen=None):
    """Return defining file for `name` exported by `mod` (best effort)."""
    seen = seen or set()
    if (mod, name) in seen or mod not in inv:
        return mod
    seen.add((mod, name))
    ex = inv[mod]["exports"]
    if name in ex:
        kind = ex[name]
        if kind.startswith("local:"):
            local = kind[6:]
            ib = imported_binding(mod, local)
            if ib:
                return resolve_symbol(ib[0], ib[1], seen)
        return mod
    for imp in reexports(mod):
        for a, b, t in imp["names"]:
            if a not in ("*", "*ns") and b == name:
                return resolve_symbol(imp["res"][1], a, seen)
            if a == "*ns" and b == name:
                return imp["res"][1]
    for imp in reexports(mod):
        for a, b, t in imp["names"]:
            if a == "*":
                tgt = imp["res"][1]
                r = resolve_symbol(tgt, name, seen)
                if r != tgt or name in inv.get(tgt, {}).get("exports", {}):
                    return r
                # check deeper star exports of tgt
                if r and r != tgt:
                    return r
    return mod


def main():
    gaps = defaultdict(list)  # target -> [(from, line, name, typeonly)]
    npm = defaultdict(set)
    node = defaultdict(set)
    for f in sorted(sel):
        for imp in inv[f]["imports"]:
            res = imp["res"]
            if res[0] == "npm":
                npm[res[1]].add(f"{f}:{imp['line']}")
                continue
            if res[0] == "node":
                node[res[1]].add(f)
                continue
            if res[0] != "file":
                gaps[str(res)].append((f, imp["line"], imp["spec"], False))
                continue
            tgt = res[1]
            if not imp["names"]:
                if tgt not in sel:
                    gaps[tgt].append((f, imp["line"], "<" + imp["kind"] + ">", False))
                continue
            for a, b, t in imp["names"]:
                if a in ("*", "*ns"):
                    if tgt not in sel:
                        gaps[tgt].append((f, imp["line"], a + " " + b, t or "type" in imp["kind"]))
                    continue
                d = resolve_symbol(tgt, a)
                if d not in sel:
                    gaps[d].append((f, imp["line"], a, t or "type" in imp["kind"]))
    for tgt in sorted(gaps):
        lst = gaps[tgt]
        tl = inv.get(tgt, {}).get("phys", "?")
        tc = inv.get(tgt, {}).get("code", "?")
        print(f"== {tgt} ({tl} lines, {tc} code)")
        by_from = defaultdict(list)
        for f, line, name, t in lst:
            by_from[f].append(f"{name}{'(t)' if t else ''}@{line}")
        for f in sorted(by_from):
            print(f"    {f}: {', '.join(sorted(set(by_from[f])))}")
    if show_npm:
        print("\n== npm imports")
        for k in sorted(npm):
            print(f"  {k}: {', '.join(sorted(npm[k]))}")
        print("\n== node builtins")
        for k in sorted(node):
            print(f"  {k}: {len(node[k])} files")


main()
