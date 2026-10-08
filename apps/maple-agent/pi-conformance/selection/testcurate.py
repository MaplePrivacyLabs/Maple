"""Curated verdicts for upstream test files that exercise selected modules.

Usage: python3 -I -B testcurate.py tests.json out.md
T = translate whole file; P = translate part (note says which); E = live-provider
end-to-end (all cases gated on API keys); R = reference only (example extension);
A = transport adapter coverage, outside the standalone Rust port.
Files not listed are not applicable (their subject is excluded).
"""
import json
import sys

sys.dont_write_bytecode = True

AG = "packages/agent/test/"
AI = "packages/ai/test/"
CA = "packages/coding-agent/test/"
S = CA + "suite/"
RG = S + "regressions/"

CUR = {
    # pi-agent-core
    AG + "agent-loop.test.ts": ("T", "Loop events, queues, executionMode, finishTurn, prepareRequest/NextTurn, terminate, runToolCall"),
    AG + "agent.test.ts": ("T", "Agent state, queues, listener settlement"),
    AG + "e2e.test.ts": ("T", "Agent over the faux provider"),
    # pi-ai
    AI + "event-stream.test.ts": ("T", ""),
    AI + "validation.test.ts": ("T", "Tool-argument coercion and schema validation"),
    AI + "context-estimate.test.ts": ("T", "estimate + simple-options clamping"),
    AI + "overflow.test.ts": ("T", ""),
    AI + "retry.test.ts": ("T", "retryAssistantCall over faux"),
    AI + "system-message-replay.test.ts": ("T", "transcript replay"),
    AI + "text.test.ts": ("T", ""),
    AI + "uuid.test.ts": ("T", ""),
    AI + "lax-message-content.test.ts": ("T", "transform-messages null content"),
    AI + "transform-messages-copilot-openai-to-anthropic.test.ts": ("T", "cross-model replay fixtures (no excluded module imported)"),
    AI + "openai-completions-raw-stop-reason.test.ts": ("T", ""),
    AI + "openai-completions-reasoning-details.test.ts": ("T", ""),
    AI + "openai-completions-retry.test.ts": ("A", "asserts SDK retry options and `retryProviderRequest`; becomes a Maple adapter test"),
    AI + "openai-completions-thinking-as-text.test.ts": ("T", ""),
    AI + "openai-completions-cache-control-format.test.ts": ("T", "catalog `getModel`: use fixture models"),
    AI + "openai-completions-prompt-cache.test.ts": ("T", "catalog `getModel`: use fixture models"),
    AI + "openai-completions-tool-choice.test.ts": ("T", "catalog `getModel`: use fixture models"),
    AI + "openai-completions-tool-result-images.test.ts": ("T", "catalog `getModel`: use fixture models"),
    AI + "openai-completions-vllm-priority.test.ts": ("T", "catalog `getModel`: use fixture models"),
    AI + "openai-completions-empty-tools.test.ts": ("T", "catalog `getModel`: use fixture models"),
    AI + "openai-completions-provider-stream-event.test.ts": ("T", "catalog `getModel`: use fixture models"),
    AI + "openai-completions-response-model.test.ts": ("T", "catalog `getModel`: use fixture models"),
    AI + "openai-completions-thinking-token-budget.test.ts": ("T", "catalog `getModel`: use fixture models"),
    AI + "faux-provider.test.ts": ("T", "uses compat `registerFauxProvider`; re-target at the Rust faux handle"),
    AI + "transcript-tool-changes.test.ts": ("P", "openai-completions cases; anthropic/responses cases go"),
    AI + "cache-retention.test.ts": ("P", "openai-completions cases only"),
    AI + "constrained-sampling.test.ts": ("P", "completions cases; Responses conversion cases go"),
    AI + "supports-xhigh.test.ts": ("P", "thinking-level clamps against vendor catalog models: rewrite with fixtures"),
    AI + "sampling-options.test.ts": ("P", "openai-completions sampling params"),
    AI + "pre-generation-error.test.ts": ("P", "openai-completions case only"),
    # pi-coding-agent: sessions and compaction
    CA + "session-manager/build-context.test.ts": ("T", "Context reconstruction from session entries"),
    CA + "session-manager/load-entries.test.ts": ("T", ""),
    CA + "session-manager/custom-session-id.test.ts": ("T", ""),
    CA + "session-manager/file-operations.test.ts": ("T", "JSONL file I/O"),
    CA + "session-manager/labels.test.ts": ("T", ""),
    CA + "session-manager/migration.test.ts": ("T", ""),
    CA + "session-manager/save-entry.test.ts": ("T", ""),
    CA + "session-manager/tree-traversal.test.ts": ("T", "tree API (kept; navigation UI is not)"),
    CA + "session-context-edit.test.ts": ("T", "Session context edits"),
    CA + "session-cwd.test.ts": ("T", ""),
    CA + "session-file-invalid.test.ts": ("T", ""),
    CA + "sdk-session-manager.test.ts": ("T", ""),
    CA + "compaction.test.ts": ("T", "2 cases live-gated (LLM summarization)"),
    CA + "compaction-serialization.test.ts": ("T", ""),
    CA + "compaction-nested-calls.test.ts": ("T", "serializes `nestedCalls` records (type kept)"),
    CA + "compaction-summary-reasoning.test.ts": ("T", ""),
    CA + "agent-session-auto-compaction-queue.test.ts": ("T", ""),
    CA + "agent-session-concurrent.test.ts": ("T", ""),
    CA + "agent-session-retry.test.ts": ("T", ""),
    CA + "agent-session-runtime-events.test.ts": ("T", ""),
    CA + "agent-session-stats.test.ts": ("T", ""),
    CA + "agent-session-dynamic-tools.test.ts": ("P", "uses the built-in bash tool; swap in a test tool"),
    CA + "agent-session-compaction.test.ts": ("E", ""),
    CA + "agent-session-branching.test.ts": ("E", ""),
    CA + "compaction-extensions.test.ts": ("E", ""),
    # tree navigation runtime
    CA + "branch-summarization.test.ts": ("T", ""),
    CA + "branch-summary-extensions.test.ts": ("T", ""),
    CA + "agent-session-tree-navigation.test.ts": ("E", ""),
    CA + "export-jsonl-share.test.ts": ("P", "JSONL export case; share goes"),
    CA + "system-prompt.test.ts": ("T", ""),
    CA + "system-prompt-updates.test.ts": ("T", ""),
    CA + "default-tools-setting.test.ts": ("P", "assumes built-in `read`/`bash`/`edit`/`write`; use test tools"),
    CA + "sdk-stream-options.test.ts": ("P", "timeout/attribution cases go"),
    CA + "sdk-skills.test.ts": ("T", ""),
    # extensions and hooks
    CA + "extensions-runner.test.ts": ("P", "Hook runner cases; shortcut, renderer, flag and provider-registration cases are excluded"),
    CA + "extensions-input-event.test.ts": ("T", ""),
    CA + "trigger-compact-extension.test.ts": ("R", "TypeScript example extension; rewrite as a Rust-native extension test"),
    CA + "compaction-extensions-example.test.ts": ("R", "TypeScript example extension; rewrite as a Rust-native extension test"),
    # resources, settings, trust
    CA + "skills.test.ts": ("T", ""),
    CA + "prompt-templates.test.ts": ("T", ""),
    CA + "resource-loader.test.ts": ("P", "theme and TypeScript-file extension cases go"),
    CA + "settings-manager.test.ts": ("P", "Agent settings; theme, terminal, TUI, network, cache-warming, shell and device-ID cases are excluded"),
    CA + "settings-manager-bug.test.ts": ("T", ""),
    CA + "settings-manager-compaction.test.ts": ("T", ""),
    CA + "settings-diagnostics.test.ts": ("T", ""),
    CA + "trust-manager.test.ts": ("T", ""),
    CA + "frontmatter.test.ts": ("T", ""),
    CA + "paths.test.ts": ("T", "1 case gated"),
    CA + "package-manager.test.ts": ("P", "Resolve, skill metadata, `.agents/skills`, ignore files, top-level patterns, force include/exclude; installer cases are excluded"),
    # tools, MCP, images
    CA + "mcp-extension.test.ts": ("P", "`MCP tools` block; config, connection and servers-section cases are excluded"),
    CA + "tool-result-images.test.ts": ("T", ""),
    CA + "image-process.test.ts": ("T", ""),
    CA + "image-processing.test.ts": ("T", "assert dimensions/MIME, not Photon's exact bytes"),
    CA + "image-resize-callers.test.ts": ("P", "read tool and `@file` cases go"),
    CA + "block-images.test.ts": ("P", "SettingsManager cases; read tool and `processFileArguments` cases go"),
    # suite (harness-based)
    S + "agent-session-boundaries.test.ts": ("T", ""),
    S + "agent-session-compaction.test.ts": ("T", ""),
    S + "agent-session-compaction-model-overrides.test.ts": ("T", ""),
    S + "agent-session-model-extension.test.ts": ("T", ""),
    S + "agent-session-prompt.test.ts": ("T", ""),
    S + "agent-session-queue.test.ts": ("T", ""),
    S + "agent-session-retry-events.test.ts": ("T", ""),
    S + "agent-session-runtime.test.ts": ("T", ""),
    S + "agent-session-tool-result-images.test.ts": ("T", ""),
    S + "agent-session-tool-orchestration.test.ts": ("P", "codemode and tool_search cases go"),
    S + "agent-session-mcp.test.ts": ("P", "drives Pi's MCP extension; re-target at Maple's MCP extension"),
    S + "lax-message-content.test.ts": ("T", ""),
    RG + "1717-2113-agent-session-event-settlement.test.ts": ("T", ""),
    RG + "2023-queued-slash-command-followup.test.ts": ("T", ""),
    RG + "2753-reload-stale-resource-settings.test.ts": ("T", ""),
    RG + "2781-skill-collision-precedence.test.ts": ("T", ""),
    RG + "2835-tools-allowlist-filters-extension-tools.test.ts": ("T", ""),
    RG + "2860-replaced-session-context.test.ts": ("T", ""),
    RG + "3317-network-connection-lost-retry.test.ts": ("T", ""),
    RG + "3592-no-builtin-tools-keeps-extension-tools.test.ts": ("T", "the `noTools: \"builtin\"` path Maple's tools use"),
    RG + "3616-settings-inmemory-reload.test.ts": ("T", ""),
    RG + "3686-session-name-event.test.ts": ("T", ""),
    RG + "3688-tree-cancel-compacting.test.ts": ("T", ""),
    RG + "3982-message-end-cost-override.test.ts": ("T", ""),
    RG + "5109-exclude-tools.test.ts": ("T", ""),
    RG + "5217-compaction-reason.test.ts": ("T", ""),
    RG + "5303-bash-output-truncation.test.ts": ("T", "`waitForChildProcess` post-exit grace, which the Rust `pi.exec` keeps; fake child and fake timers"),
    RG + "5996-session-name-newlines.test.ts": ("T", ""),
    RG + "5998-blocked-tool-terminate.test.ts": ("T", ""),
    RG + "6019-explicit-provider-retry-message.test.ts": ("T", ""),
    RG + "6162-extension-active-tools-next-turn.test.ts": ("T", ""),
    RG + "6260-inline-extension-naming.test.ts": ("T", ""),
    RG + "6324-branch-summary-ambient-auth.test.ts": ("T", ""),
    RG + "6363-agent-settled-event.test.ts": ("T", ""),
    RG + "6647-compaction-retries-transient-stream-drop.test.ts": ("T", ""),
    RG + "6768-copilot-compaction-base-url.test.ts": ("P", "auth `baseUrl` override in summaries; vendor model fixture"),
    RG + "6904-dns-transport-retry.test.ts": ("T", ""),
    RG + "7048-compaction-truncated-summary.test.ts": ("T", ""),
    RG + "7150-rpc-prompt-during-compaction.test.ts": ("T", "AgentSession-level despite the name"),
    RG + "7193-event-bus-lifecycle.test.ts": ("T", ""),
    RG + "7253-manual-compact-during-response.test.ts": ("T", ""),
    RG + "7301-stalled-availability-refresh.test.ts": ("P", "model-runtime availability refresh; re-target at the seam"),
    RG + "7497-session-discovery-symlink.test.ts": ("T", "1 case gated"),
    RG + "7572-provider-retry-settings-merge.test.ts": ("T", ""),
    RG + "8328-zero-usage-auto-compaction.test.ts": ("T", ""),
    RG + "8337-utf8-bom-parsing.test.ts": ("T", ""),
    RG + "8423-extension-factory-failure.test.ts": ("T", ""),
    RG + "8537-custom-message-tool-result-ordering.test.ts": ("T", ""),
    RG + "8724-in-memory-fork-active-tool.test.ts": ("T", ""),
    RG + "8935-parallel-preflight-abort.test.ts": ("T", ""),
    RG + "8989-fork-compaction-label-boundary.test.ts": ("T", ""),
    RG + "9178-tree-during-compaction.test.ts": ("T", ""),
    RG + "9340-9777-auto-compaction-cancellation.test.ts": ("T", ""),
    RG + "9789-context-handler-system-messages.test.ts": ("T", ""),
    RG + "pre-prompt-compaction-no-continue.test.ts": ("T", ""),
    RG + "tree-during-streaming.test.ts": ("T", ""),
}

GROUPS = [
    ("pi-agent-core", [k for k in CUR if k.startswith(AG)]),
    ("pi-ai", [k for k in CUR if k.startswith(AI)]),
    ("pi-coding-agent (top-level and `session-manager/`)", [k for k in CUR if k.startswith(CA) and not k.startswith(S)]),
    ("pi-coding-agent `suite/` (faux provider plus `suite/harness.ts`)", [k for k in CUR if k.startswith(S)]),
]


def main():
    r = json.load(open(sys.argv[1]))
    lines = []
    totals = {}
    for title, keys in GROUPS:
        lines.append(f"\n#### {title}\n")
        lines.append("| Test file | Tests | `.each` | Live-gated | Generated data | Use | Note |")
        lines.append("|---|---:|---:|---:|---|---|---|")
        t = {"T": [0, 0], "P": [0, 0], "E": [0, 0], "R": [0, 0], "A": [0, 0]}
        gen_files = 0
        for k in sorted(keys):
            v = r[k]
            verdict, note = CUR[k]
            t[verdict][0] += 1
            t[verdict][1] += v["active"]
            gen = "Yes" if v["needs_generated"] else "No"
            gen_files += v["needs_generated"]
            name = k.split("/test/", 1)[1]
            lines.append(f"| `{name}` | {v['active']} | {v['each'] or ''} | {min(v['gated'], v['active']) or ''} | {gen} | {verdict} | {note} |")
        totals[title] = (t, gen_files, len(keys))
    out = "\n".join(lines)
    open(sys.argv[2], "w").write(out)
    for k, (t, g, n) in totals.items():
        print(k, n, "files;", {v: f"{c[0]} files/{c[1]} tests" for v, c in t.items() if c[0]}, "generated:", g)
    missing = [k for k in CUR if k not in r]
    print("missing", missing)


if __name__ == "__main__":
    main()
