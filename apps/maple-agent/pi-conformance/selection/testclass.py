"""Classify upstream test files against the selection.

Usage: python3 -I testclass.py tests.json out.json
Verdicts:
  Translate - subject is selected code; excluded imports are only harness
              infrastructure that the Rust test harness replaces
  Partial   - subject is selected, but some cases use excluded features
  Skip      - subject is excluded (or type-level only)
"""
import json
import re
import sys

r = json.load(open(sys.argv[1]))
TYPES = {"packages/ai/src/types.ts", "packages/agent/src/types.ts"}
# Excluded modules the Rust harness replaces (faux registration, test model runtime, auth for it)
INFRA = {
    "packages/ai/src/compat.ts", "packages/ai/src/providers/all.ts",
    "packages/coding-agent/src/core/auth-storage.ts", "packages/coding-agent/src/core/model-runtime.ts",
    "packages/coding-agent/src/core/models-store.ts", "packages/ai/src/auth/credential-store.ts",
    "packages/ai/src/models-store.ts", "packages/ai/src/index.ts", "packages/coding-agent/src/index.ts",
    "packages/ai/src/utils/typebox-helpers.ts",
}
SKIP_NAME = re.compile(
    r"(anthropic|bedrock|google|mistral|azure|codex|openai-responses|xai|openrouter|cloudflare|github-copilot|"
    r"kimi|meta-oauth|radius|oauth|fireworks|baseten|together|qwen|xiaomi|zai|zen|images?-|classifier|llama|"
    r"typesafe|pi-messages|env-api-keys|error-body|provider-retry|node-http-proxy|model-catalog|model-data|"
    r"generate-models|reasoning-options|lazy-module|models-entry|message-types|telemetry|fetch-option|"
    r"tree-selector|codemode|tool-search|nested-tool|bash|powershell|find|grep|ls-tool|"
    r"edit-tool|edit-diff|file-mutation|path-utils|read-tool|tools-manager|export-html|rpc|interactive|tui|theme|"
    r"keybinding|clipboard|footer-width|model-runtime|model-registry|model-resolver|scoped-model|model-selector|"
    r"cache-warmer|cache-stats|auth-storage|auth-check|credential|runtime-credentials|resolve-config|"
    r"virtual-model|jev-router|experimental|changelog|crash-log|bug-report|args|cli|startup|version|"
    r"migration|package-command|package-distribution|management-http|http-dispatcher|pi-user-agent|"
    r"print-mode|json-stream|json-event|initial-message|first-time|max-thinking|mermaid|ansi|sea-|"
    r"restore-sandbox|external-editor|collapsible|assistant-message|custom-message|chat-viewport|"
    r"documentation|mcp-command|mcp-oauth|plan-mode|git-|abort|stream\.test|empty\.test|unicode|tokens\.test|"
    r"total-tokens|responseid|tool-call-without|tool-call-id|cross-provider|interleaved|context-overflow|"
    r"image-tool-result|sampling-options|supports-xhigh|xhigh|session-picker|resume|share|login|"
    r"signal|sigterm|taskkill|uppercase-header|missing-theme|thinking-toggle|extension-oauth|"
    r"models-store|remote-catalog|session-id-readonly|format-resume|config-value|dynamic-provider|"
    r"openrouter-attribution|extensions-discovery|extension-loader-lazy|extension-factory-cache|"
    r"node-sea|subagent|input-transform-streaming|git-merge|compaction-extensions-example|"
    r"unknown-command|end-of-options|queued-slash|session-start-notify|invalid-settings|resource-loader-theme|"
    r"fswatch|block-images|image-resize-callers|builtin-tool-strict|default-tools|no-builtin-tools|"
    r"tools-allowlist|exclude-tools|inline-extension-naming|mcp-tool-renderers|stalled-availability|"
    r"models-json|credential-refresh|copilot-compaction|scoped-models-refresh|user-bash|late-bash|"
    r"bash-output|lax-message-content|transcript-tool-changes)",
)


def verdict(path, v):
    name = path.split("/test/", 1)[1]
    subj = [s for s in v["selected_used"] if s not in TYPES]
    excl = [e for e in v["excluded_used"] if e not in INFRA and "/test/" not in e]
    harness = any(h.endswith("suite/harness.ts") for h in v["helpers"])
    if not subj and not harness:
        return "Skip", subj, excl
    if excl:
        return "Partial", subj, excl
    return "Translate", subj, excl


out = {}
for path, v in sorted(r.items()):
    vd, subj, excl = verdict(path, v)
    out[path] = dict(verdict=vd, subj=subj, excl=excl, **{k: v[k] for k in ("active", "skipped", "each", "gated", "needs_generated", "pkg", "phys")})
json.dump(out, open(sys.argv[2], "w"), indent=1, sort_keys=True)
from collections import Counter
c = Counter((v["pkg"], v["verdict"]) for v in out.values())
for k in sorted(c):
    n = sum(v["active"] for p, v in out.items() if (v["pkg"], v["verdict"]) == k)
    print(k, c[k], "files", n, "static tests")
