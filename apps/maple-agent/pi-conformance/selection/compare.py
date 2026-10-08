"""Compare the selection with the historical whole-file baseline.

The whole and split tables below define the 27,768-line baseline in full;
code counts for split files are prorated by their physical line counts.
Usage: python3 -I compare.py inv.json measured.json
"""
import json
import sys

inv = json.load(open(sys.argv[1]))
meas = {r["path"]: r for r in json.load(open(sys.argv[2]))["rows"]}
A = "packages/agent/src/"
I = "packages/ai/src/"
C = "packages/coding-agent/src/"
whole = [A + f for f in ["agent-loop.ts", "agent.ts", "types.ts", "stream-fn.ts"]]
whole += [I + f for f in [
    "types.ts", "utils/event-stream.ts", "utils/json-parse.ts", "utils/validation.ts",
    "utils/transcript.ts", "utils/estimate.ts", "utils/diagnostics.ts", "utils/text.ts",
    "utils/hash.ts", "utils/headers.ts", "utils/sanitize-unicode.ts",
    "api/transform-messages.ts", "api/openai-completions.ts", "api/simple-options.ts",
    "utils/overflow.ts", "utils/retry.ts", "providers/faux.ts"]]
whole += [C + f for f in [
    "core/messages.ts", "core/session-cwd.ts", "core/defaults.ts", "core/event-bus.ts",
    "core/compaction/compaction.ts", "core/compaction/utils.ts", "core/compaction/index.ts",
    "core/extensions/wrapper.ts", "core/tools/tool-definition-wrapper.ts",
    "core/skills.ts", "core/prompt-templates.ts", "core/source-info.ts", "core/diagnostics.ts",
    "utils/frontmatter.ts", "utils/paths.ts", "utils/text.ts", "utils/output-files.ts",
    "core/sdk.ts", "core/agent-session-runtime.ts", "core/agent-session-services.ts",
    "core/system-prompt.ts", "core/usage-totals.ts", "core/model-registry.ts", "utils/sleep.ts",
    "core/settings-diagnostics.ts", "core/project-trust.ts", "core/trust-manager.ts",
    "core/tools/index.ts", "core/tools/truncate.ts",
    "utils/image-process.ts", "utils/image-convert.ts", "utils/image-resize.ts",
    "utils/image-resize-core.ts", "utils/image-resize-worker.ts", "utils/exif-orientation.ts",
    "utils/photon.ts", "utils/mime.ts", "utils/tool-result-images.ts",
    "extensions/mcp/index.ts", "extensions/mcp/tools.ts", "core/mcp-servers.ts"]]
split = {
    C + "core/agent-session.ts": 2919 + 919,
    C + "core/session-manager.ts": 1356,
    C + "core/extensions/types.ts": 1886,
    C + "core/extensions/runner.ts": 1435,
    C + "core/extensions/loader.ts": 394,
    C + "core/resource-loader.ts": 841,
    C + "core/package-manager.ts": 806,
    C + "core/settings-manager.ts": 1150,
    C + "config.ts": 79,
}
ps = {}
for f in whole:
    ps[f] = (inv[f]["phys"], inv[f]["code"])
for f, part in split.items():
    ps[f] = (part, inv[f]["code"] * part / inv[f]["phys"])
sel = {p: (r["sel_phys"], r["sel_code"]) for p, r in meas.items() if r["cls"] != "Exclude"}
rows = []
for f in sorted(set(ps) | set(sel)):
    a = ps.get(f, (0, 0))
    b = sel.get(f, (0, 0))
    if round(a[0]) != round(b[0]):
        rows.append((b[0] - a[0], b[1] - a[1], f, a, b))
tp = sum(v[0] for v in ps.values())
tc = sum(v[1] for v in ps.values())
sp = sum(v[0] for v in sel.values())
sc = sum(v[1] for v in sel.values())
print(f"historical baseline: {tp} lines / {tc:.0f} code; selection: {sp} lines / {sc} code; delta {sp - tp} / {sc - tc:.0f}")
for d, dc, f, a, b in sorted(rows):
    print(f"{d:+6} lines {dc:+7.0f} code  {f.replace('packages/', '')}  ({a[0]} -> {b[0]})")
