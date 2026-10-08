"""For each selected file, list imports that resolve to non-selected files
(symbol-level, through barrels), split into: referenced by kept code, or
referenced only by cut code. Also lists npm and node imports used by kept code.

Usage: python3 -I deps.py ROOT inv.json out.json
"""
import json
import os
import re
import subprocess
import sys
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import classes  # noqa: E402
import tslex  # noqa: E402

ROOT, INV, OUT = sys.argv[1:4]
inv = json.load(open(INV))
SEL = set(classes.SELECTED)


def reexports(mod):
    return [i for i in inv[mod]["imports"] if i["kind"].startswith("export") and i["res"][0] == "file"]


def imported_binding(mod, local):
    for imp in inv[mod]["imports"]:
        if imp["kind"].startswith("import") and imp["res"][0] == "file":
            for a, b, t in imp["names"]:
                if b == local:
                    return imp["res"][1], a
    return None


def resolve_symbol(mod, name, seen=None):
    seen = seen or set()
    if (mod, name) in seen or mod not in inv:
        return mod
    seen.add((mod, name))
    ex = inv[mod]["exports"]
    if name in ex:
        kind = ex[name]
        if kind.startswith("local:"):
            ib = imported_binding(mod, kind[6:])
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
    return mod


def members(path):
    out = subprocess.check_output([sys.executable, "-I", "-B", os.path.join(HERE, "members.py"), path]).decode()
    res = []
    for line in out.splitlines():
        m = re.match(r"\s*(\d+)-(\d+)\s+(\d+)\s+(\d+)\s+(.*)$", line)
        if m:
            res.append((int(m.group(1)), int(m.group(2)), m.group(5).split()[-1]))
    return res


def rng(spec):
    a, b = spec.split("-")
    return set(range(int(a), int(b) + 1))


def cut_symbols(rel, spec):
    """Names of top-level declarations/members removed from a selected file."""
    full = os.path.join(ROOT, rel)
    mem = members(full)
    names = set(spec.get("cut", []))
    if spec.get("keep"):
        keep = set()
        for r in spec["keep"]:
            keep |= rng(r)
        for s_, e_, name in mem:
            if not (set(range(s_, e_ + 1)) & keep):
                names.add(name)
    for r in spec.get("ranges", []):
        rr = rng(r)
        for s_, e_, name in mem:
            if set(range(s_, e_ + 1)) <= rr:
                names.add(name)
    return names


CUTSYMS = {rel: cut_symbols(rel, spec) for rel, spec in classes.SELECTED.items() if spec.get("cut") or spec.get("keep") or spec.get("ranges")}

result = {}
for rel, spec in sorted(classes.SELECTED.items()):
    full = os.path.join(ROOT, rel)
    text = open(full, encoding="utf-8").read()
    raw = text.split("\n")
    clean = tslex.strip_comments_and_strings(text).split("\n")
    n = len(raw)
    if spec.get("keep"):
        keep = set()
        for r in spec["keep"]:
            keep |= rng(r)
        cut_lines = set(range(1, n + 1)) - keep
    else:
        cut_lines = set()
        names = set(spec.get("cut", []))
        if names:
            for s, e, name in members(full):
                if name in names:
                    cut_lines |= set(range(s, e + 1))
        for r in spec.get("ranges", []):
            cut_lines |= rng(r)
    import_lines = set()
    for imp in inv[rel]["imports"]:
        if not imp["kind"].startswith(("import", "export")):
            continue
        j = imp["line"]
        while j <= n:
            import_lines.add(j)
            if re.search(r"""\bfrom\s*['"]""", raw[j - 1]) or re.match(r"""\s*import\s*['"]""", raw[j - 1]):
                break
            j += 1
    kept_text = "\n".join(l for i, l in enumerate(clean, 1) if i not in cut_lines and i not in import_lines)
    deps = defaultdict(lambda: {"kept": set(), "cut": set()})
    ext = defaultdict(lambda: {"kept": set(), "cut": set()})
    for imp in inv[rel]["imports"]:
        res = imp["res"]
        names = imp["names"] or [("<module>", "<module>", False)]
        for a, b, t in names:
            if a == "*":
                continue
            if b == "<module>":
                used = imp["line"] not in cut_lines
            else:
                used = bool(re.search(r"(?<![\w$.])" + re.escape(b) + r"(?![\w$])", kept_text))
            bucket = "kept" if used else "cut"
            label = a if a != b else a
            if t or "type" in imp["kind"]:
                label += " (type)"
            if res[0] == "file":
                tgt = res[1] if a in ("<module>", "*ns") else resolve_symbol(res[1], a)
                if tgt in SEL:
                    if a in CUTSYMS.get(tgt, ()):
                        deps[f"{tgt} [cut part]"][bucket].add(f"{label}@{imp['line']}")
                    continue
                deps[f"{tgt}"][bucket].add(f"{label}@{imp['line']}")
            elif res[0] in ("npm", "node"):
                ext[res[1]][bucket].add(label)
            else:
                deps[str(res)][bucket].add(f"{label}@{imp['line']}")
    result[rel] = {
        "deps": {k: {"kept": sorted(v["kept"]), "cut": sorted(v["cut"])} for k, v in deps.items()},
        "ext": {k: {"kept": sorted(v["kept"]), "cut": sorted(v["cut"])} for k, v in ext.items()},
    }
json.dump(result, open(OUT, "w"), indent=1, sort_keys=True)
for rel, v in result.items():
    kept = {k: d["kept"] for k, d in v["deps"].items() if d["kept"]}
    if kept:
        print("==", rel.replace("packages/", ""))
        for k, names in sorted(kept.items()):
            print("    KEPT-REF", k.replace("packages/", ""), ":", ", ".join(names))
