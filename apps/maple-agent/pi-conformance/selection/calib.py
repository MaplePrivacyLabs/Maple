"""Measure the historical whole-file baseline (27,768 physical lines).

The whole and split tables below define this comparison baseline in full.
This is calibration data, not the current selected scope.
Usage: python3 -I calib.py ROOT inv.json
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import tslex  # noqa: E402

ROOT = sys.argv[1]
inv = json.load(open(sys.argv[2]))

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

split = {  # file: selected part lines
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

tl = tc = 0
for f in whole:
    tl += inv[f]["phys"]
    tc += inv[f]["code"]
print("whole files", len(whole), tl, tc)
sl = sc = 0.0
for f, part in split.items():
    sl += part
    sc += inv[f]["code"] * part / inv[f]["phys"]
print("split parts", sl, round(sc, 1))
print("TOTAL lines", tl + sl, "code (prorated)", round(tc + sc, 1))
# exact range for config.ts
text = open(os.path.join(ROOT, C + "config.ts")).read()
_, _, pl = tslex.analyze(text)
print("config.ts 578-656 exact code", sum(pl[577:656]), "prorated", inv[C + "config.ts"]["code"] * 79 / 656)
