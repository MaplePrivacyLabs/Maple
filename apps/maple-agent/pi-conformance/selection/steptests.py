"""Assign each curated upstream test file to the first porting step at which
all the selected modules it reaches (directly or through test helpers) exist.

Usage: python3 -I steptests.py inv_all.json tests.json testcurate.py steps.py
The CUR table is read from testcurate.py and STEPS from steps.py (both are this
directory's own scripts); only their literal assignment blocks are evaluated.
"""
import json
import sys
from collections import defaultdict

INV_ALL, TESTS, CURATE, STEPSRC = sys.argv[1:5]
inv = json.load(open(INV_ALL))
tests = json.load(open(TESTS))


ns ={"AG": "packages/agent/test/", "AI": "packages/ai/test/", "CA": "packages/coding-agent/test/"}
ns["S"] = ns["CA"] + "suite/"
ns["RG"] = ns["S"] + "regressions/"
src = open(CURATE).read()
a = src.index("CUR = {")
b = src.index("\nGROUPS")
exec(src[a:b], ns)
CUR = ns["CUR"]
ns2 = {"A": "packages/agent/src/", "I": "packages/ai/src/", "C": "packages/coding-agent/src/"}
s2 = open(STEPSRC).read()
a = s2.index("STEPS = {")
b = s2.index("\nSTEP = ")
exec(s2[a:b], ns2)
STEPS = ns2["STEPS"]
STEP = {f: s for s, fs in STEPS.items() for f in fs}


def reexports(mod):
    return [i for i in inv[mod]["imports"] if i["kind"].startswith("export") and i["res"][0] == "file"]


def imported_binding(mod, local):
    for imp in inv[mod]["imports"]:
        if imp["kind"].startswith("import") and imp["res"][0] == "file":
            for x, y, t in imp["names"]:
                if y == local:
                    return imp["res"][1], x
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
        for x, y, t in imp["names"]:
            if x not in ("*", "*ns") and y == name:
                return resolve_symbol(imp["res"][1], x, seen)
            if x == "*ns" and y == name:
                return imp["res"][1]
    for imp in reexports(mod):
        for x, y, t in imp["names"]:
            if x == "*":
                tgt = imp["res"][1]
                r = resolve_symbol(tgt, name, seen)
                if r != tgt or name in inv.get(tgt, {}).get("exports", {}):
                    return r
    return mod


def subjects(test):
    """Selected and excluded src modules reached from the test and its helpers."""
    sel, exc = set(), set()
    todo, seen = [test], set()
    while todo:
        f = todo.pop()
        if f in seen or f not in inv:
            continue
        seen.add(f)
        for imp in inv[f]["imports"]:
            if imp["res"][0] != "file":
                continue
            tgt0 = imp["res"][1]
            if "/test/" in tgt0:
                todo.append(tgt0)
                continue
            for x, y, t in imp["names"] or [("<module>", "<module>", False)]:
                tgt = resolve_symbol(tgt0, x) if x not in ("*", "*ns", "<module>", "default") else tgt0
                (sel if tgt in STEP else exc).add(tgt)
    return sel, exc


# Manual corrections, each checked against the test's own imports:
# - compat stream/streamSimple/complete(Simple) calls with openai-completions
#   models exercise the step-2 request builder and parser;
# - the read-tool and @file cases of image-resize-callers go (P), and what is
#   left tests resizeImage (step 5);
# - file-operations and tree-traversal take only three message helpers from
#   test/utilities.ts, whose other imports reach AgentSession;
# - the adapter test (A) belongs with Maple's adapter (step 8);
# - regression 5303 imports only waitForChildProcess, which core/exec.ts
#   (step 6) replaces in Rust, so it reaches no selected module.
AI_T = "packages/ai/test/"
CA_T = "packages/coding-agent/test/"
OVERRIDES = {
    AI_T + "openai-completions-empty-tools.test.ts": 2,
    AI_T + "openai-completions-provider-stream-event.test.ts": 2,
    AI_T + "openai-completions-response-model.test.ts": 2,
    AI_T + "openai-completions-thinking-token-budget.test.ts": 2,
    AI_T + "sampling-options.test.ts": 2,
    AI_T + "transcript-tool-changes.test.ts": 2,
    AI_T + "openai-completions-retry.test.ts": 8,
    CA_T + "image-resize-callers.test.ts": 5,
    CA_T + "session-manager/file-operations.test.ts": 4,
    CA_T + "session-manager/tree-traversal.test.ts": 4,
    CA_T + "suite/regressions/5303-bash-output-truncation.test.ts": 6,
}

by_step = defaultdict(list)
for t, (verdict, note) in CUR.items():
    sel, exc = subjects(t)
    step = max((STEP[f] for f in sel), default=0)
    step = OVERRIDES.get(t, step)
    by_step[step].append((t, verdict, tests[t]["active"], tests[t]["needs_generated"], sorted(exc)))
total = defaultdict(lambda: [0, 0])
for s in sorted(by_step):
    agg = defaultdict(lambda: [0, 0])
    for t, v, n, g, exc in by_step[s]:
        agg[v][0] += 1
        agg[v][1] += n
        total[v][0] += 1
        total[v][1] += n
    gen = sum(1 for r in by_step[s] if r[3])
    print(f"SUMMARY step {s}: {len(by_step[s])} files, {sum(r[2] for r in by_step[s])} tests, generated {gen}; "
          + "; ".join(f"{v} {c[0]}/{c[1]}" for v, c in sorted(agg.items())))
print("SUMMARY total", {v: c for v, c in sorted(total.items())})

for s in sorted(by_step):
    rows = by_step[s]
    agg = defaultdict(lambda: [0, 0])
    for t, v, n, g, exc in rows:
        agg[v][0] += 1
        agg[v][1] += n
    print(f"== step {s}: {len(rows)} files; " + "; ".join(f"{v} {c[0]} files/{c[1]} tests" for v, c in sorted(agg.items())))
    for t, v, n, g, exc in sorted(rows):
        name = t.split("/test/", 1)[1]
        ex = [e.replace("packages/", "") for e in exc if not e.endswith(".json")]
        print(f"   {v} {n:4d} {'G' if g else ' '} {name}" + (f"   [excluded: {', '.join(ex)}]" if ex else ""))
