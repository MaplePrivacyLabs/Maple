"""Porting-order check: assigns each selected file to a step, prints per-step
totals, and lists imports (symbol-level, through barrels) from a selected file
to a selected file in a LATER step. Type-only imports are reported separately.

Usage: python3 -I steps.py inv.json measured.json
"""
import json
import sys
from collections import defaultdict

inv = json.load(open(sys.argv[1]))
meas = {r["path"]: r for r in json.load(open(sys.argv[2]))["rows"] if r["cls"] != "Exclude"}

A = "packages/agent/src/"
I = "packages/ai/src/"
C = "packages/coding-agent/src/"
STEPS = {
    1: [I + f for f in [
        "types.ts", "models.ts", "auth/types.ts", "session-resources.ts",
        "utils/event-stream.ts", "utils/json-parse.ts", "utils/validation.ts",
        "utils/transcript.ts", "utils/estimate.ts", "utils/diagnostics.ts", "utils/text.ts",
        "utils/hash.ts", "utils/headers.ts", "utils/sanitize-unicode.ts", "utils/uuid.ts",
        "utils/model-operations.ts", "utils/overflow.ts", "utils/retry.ts", "providers/faux.ts"]],
    2: [I + f for f in [
        "api/transform-messages.ts", "api/simple-options.ts", "api/constrained-sampling.ts",
        "api/openai-prompt-cache.ts", "utils/provider-env.ts", "api/openai-completions.ts"]],
    3: [A + f for f in ["types.ts", "stream-fn.ts", "agent-loop.ts", "agent.ts"]],
    4: [C + f for f in [
        "core/messages.ts", "core/session-manager.ts", "core/session-cwd.ts",
        "core/session-export.ts", "core/compaction/compaction.ts", "core/compaction/utils.ts",
        "core/compaction/branch-summarization.ts",
        "config.ts", "utils/paths.ts", "core/defaults.ts", "core/usage-totals.ts", "utils/text.ts"]],
    5: [C + f for f in [
        "core/settings-manager.ts", "core/trust-manager.ts", "core/skills.ts",
        "core/prompt-templates.ts", "utils/frontmatter.ts", "core/source-info.ts",
        "core/diagnostics.ts", "core/package-manager.ts",
        "utils/image-process.ts", "utils/image-convert.ts", "utils/image-resize.ts",
        "utils/image-resize-core.ts", "utils/exif-orientation.ts", "utils/mime.ts",
        "utils/tool-result-images.ts"]],
    6: [C + f for f in [
        "core/extensions/types.ts", "core/extensions/runner.ts", "core/extensions/loader.ts",
        "core/extensions/wrapper.ts", "core/event-bus.ts", "core/exec.ts", "core/slash-commands.ts",
        "core/tools/tool-definition-wrapper.ts", "core/tools/truncate.ts", "utils/output-files.ts",
        "core/mcp-servers.ts", "core/resource-loader.ts", "core/footer-data-provider.ts", "core/project-trust.ts",
        "core/system-prompt.ts", "core/model-registry.ts"]],
    7: [C + f for f in [
        "core/agent-session.ts", "core/sdk.ts", "core/agent-session-services.ts",
        "core/agent-session-runtime.ts", "core/virtual-models.ts", "utils/sleep.ts",
        "core/settings-diagnostics.ts"]],
    8: [C + "extensions/mcp/tools.ts", "packages/mcp/src/protocol/content.ts"],
}
STEP = {f: s for s, fs in STEPS.items() for f in fs}
missing = set(meas) - set(STEP)
extra = set(STEP) - set(meas)
assert not missing and not extra, (missing, extra)


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


for s, fs in STEPS.items():
    ph = sum(meas[f]["sel_phys"] for f in fs)
    co = sum(meas[f]["sel_code"] for f in fs)
    print(f"step {s}: {len(fs)} files, {ph} lines, {co} code")
print("total", sum(len(v) for v in STEPS.values()))

viol = defaultdict(set)
for src in STEP:
    for imp in inv[src]["imports"]:
        if imp["res"][0] != "file":
            continue
        for a, b, t in imp["names"] or [("<module>", "<module>", False)]:
            tgt = resolve_symbol(imp["res"][1], a) if a not in ("*", "*ns", "<module>", "default") else imp["res"][1]
            if tgt in STEP and STEP[tgt] > STEP[src]:
                tonly = t or imp["kind"].endswith("type")
                viol[(src, tgt)].add((a, "type" if tonly else "value", imp["line"]))
for (src, tgt), names in sorted(viol.items()):
    print(f"LATER: step {STEP[src]} {src.replace('packages/', '')} -> step {STEP[tgt]} {tgt.replace('packages/', '')}: "
          + ", ".join(f"{n}({k}@{l})" for n, k, l in sorted(names)))

# Step-to-step edges (imports into earlier or same steps), to show which steps
# can proceed in parallel.
edges = defaultdict(set)
for src in STEP:
    for imp in inv[src]["imports"]:
        if imp["res"][0] != "file":
            continue
        for a, b, t in imp["names"] or [("<module>", "<module>", False)]:
            tgt = resolve_symbol(imp["res"][1], a) if a not in ("*", "*ns", "<module>", "default") else imp["res"][1]
            if tgt in STEP and STEP[tgt] != STEP[src]:
                edges[STEP[src]].add(STEP[tgt])
for s in sorted(STEPS):
    print(f"EDGES step {s} imports from steps {sorted(edges[s])}")
