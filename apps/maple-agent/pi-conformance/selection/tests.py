"""Static analysis of upstream test files against a selection.

Usage: python3 -I tests.py ROOT inv_all.json selection.txt out.json
For each test file under packages/{agent,ai,coding-agent}/test:
- static test counts (it/test calls; skipped/todo; .each flagged)
- src files whose symbols the test (and its local test helpers) use
- whether the static runtime import closure reaches gitignored generated
  provider data (providers/data/*.json)
"""
import json
import os
import re
import sys
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import tslex  # noqa: E402

ROOT, INV, SEL, OUT = sys.argv[1:5]
inv = json.load(open(INV))
sel = set(l.split("#")[0].strip() for l in open(SEL) if l.split("#")[0].strip())

TEST_CALL = re.compile(r"(?<![\w$.])(it|test)((?:\.(?:only|concurrent|sequential|fails|skipIf\([^)]*\)|runIf\([^)]*\)))*)\s*\(")
SKIP_CALL = re.compile(r"(?<![\w$.])(it|test)\.(skip|todo)\s*\(")
EACH_CALL = re.compile(r"(?<![\w$.])(it|test|describe)\.each\s*[(`]")
DESCRIBE_SKIP = re.compile(r"(?<![\w$.])describe\.skip\s*\(")


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


def runtime_closure(start):
    """Static runtime import closure (type-only imports erased)."""
    seen = set()
    missing = set()
    stack = [start]
    while stack:
        f = stack.pop()
        if f in seen or f not in inv:
            continue
        seen.add(f)
        for imp in inv[f]["imports"]:
            if imp["kind"] in ("import-type", "export-type", "dynamic"):
                continue
            res = imp["res"]
            if res[0] == "missing":
                missing.add(res[1])
            elif res[0] == "file":
                stack.append(res[1])
    return seen, missing


def is_test_file(path):
    return path.endswith(".test.ts") or path.endswith(".spec.ts")


results = {}
for path, meta in inv.items():
    if not meta["test"] or meta["pkg"] not in ("agent", "ai", "coding-agent") or not is_test_file(path):
        continue
    text = open(os.path.join(ROOT, path), encoding="utf-8").read()
    clean = tslex.strip_comments(text)
    active = len(TEST_CALL.findall(clean))
    skipped = len(SKIP_CALL.findall(clean))
    each = len(EACH_CALL.findall(clean))
    dskip = len(DESCRIBE_SKIP.findall(clean))
    # subject: symbols used from src files by the test and its local helpers
    used = defaultdict(set)
    helpers = set()
    for imp in inv[path]["imports"]:
        res = imp["res"]
        if res[0] != "file":
            continue
        tgt = res[1]
        if inv.get(tgt, {}).get("test"):
            helpers.add(tgt)
            continue
        if not imp["names"]:
            used[tgt].add("<module>")
        for a, b, t in imp["names"]:
            if a in ("*", "*ns"):
                used[tgt].add("*")
                continue
            used[resolve_symbol(tgt, a)].add(a)
    live = bool(re.search(r"process\.env\.[A-Z_]*(API_KEY|TOKEN)|getEnvApiKey|hasApiKey|skipIf\(\s*!", clean)) or "createTestSession" in clean
    # tests inside describe.skipIf(...)/describe.runIf(...) blocks or it.skipIf(...) calls
    gated = 0
    for m in re.finditer(r"(?<![\w$.])describe\.(skipIf|runIf)\s*\(", clean):
        # find the callback body: first "{" after the second "(" group
        j = clean.find("=>", m.end())
        k = clean.find("{", j)
        if j < 0 or k < 0:
            continue
        depth = 0
        end = k
        for idx in range(k, len(clean)):
            ch = clean[idx]
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    end = idx
                    break
        gated += len(TEST_CALL.findall(clean[k:end]))
    gated += len(re.findall(r"(?<![\w$.])(it|test)\.(skipIf|runIf)\s*\(", clean))
    closure, missing = runtime_closure(path)
    gen = sorted(m for m in missing if "/providers/data/" in m)
    selected_used = sorted(f for f in used if f in sel)
    excluded_used = {f: sorted(v) for f, v in used.items() if f not in sel and not inv.get(f, {}).get("test")}
    results[path] = {
        "pkg": meta["pkg"],
        "phys": meta["phys"],
        "active": active,
        "skipped": skipped,
        "each": each,
        "describe_skip": dskip,
        "helpers": sorted(helpers - {path}),
        "live": live,
        "gated": gated,
        "selected_used": selected_used,
        "excluded_used": excluded_used,
        "needs_generated": bool(gen),
        "generated_example": gen[:1],
        "reaches_generated_ts": any(p.endswith("models.generated.ts") or p.endswith(".models.ts") for p in closure),
    }
json.dump(results, open(OUT, "w"), indent=1, sort_keys=True)
print("test files", len(results))
