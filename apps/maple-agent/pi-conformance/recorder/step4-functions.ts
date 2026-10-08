/** Direct selected-source observations. Import modules only after resetting their clock state. */
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { vi } from "vitest";
import { deterministicEnvironment } from "./determinism.ts";
import type { FunctionMatrix, Input } from "./schema.ts";

export const STEP4_FUNCTION_IDS: readonly string[] = [
  "branch-summarization.collectEntriesForBranchSummary",
  "branch-summarization.generateBranchSummary",
  "branch-summarization.prepareBranchEntries",
  "compaction-utils.fileOperations",
  "compaction-utils.serializeConversation",
  "compaction.calculateContextTokens",
  "compaction.estimateContextTokens",
  "compaction.estimateProjectedContextTokens",
  "compaction.estimateTokens",
  "compaction.findCutPoint",
  "compaction.getLastAssistantUsage",
  "compaction.prepareCompaction",
  "compaction.retryRequestOwnership",
  "compaction.serializeConversation",
  "compaction.shouldCompact",
  "compaction.summarization",
  "messages.bashExecutionToText",
  "messages.construct",
  "messages.convertToLlm",
  "session-manager.buildSessionContext",
  "session.assertValidId",
  "session.cwdFormatting",
  "session.entryToMessages",
  "session.inMemory",
  "session.parseAndMigrate",
  "session.project",
  "text.bom",
  "usage.totals"
];
export async function recordStep4Function(input: Input<FunctionMatrix>, destination: string) {
  const rows: unknown[] = [];
  for (const item of input.value.cases) {
    vi.resetModules();
    const clock = deterministicEnvironment(input.value.clock.epochMs);
    try {
      let output: unknown;
      if (/^(compaction[.-]|compaction-utils\.|branch-summarization\.)/.test(input.value.id)) {
        const { dispatchCompactionFunction } = await import("./compaction-dispatch.ts");
        const pending = dispatchCompactionFunction(input.value.id, item.input);
        if (input.value.id === "compaction.retryRequestOwnership") await vi.runAllTimersAsync();
        output = await pending;
      } else {
        const { dispatchSessionFunction } = await import("./session-dispatch.ts");
        output = dispatchSessionFunction(input.value.id, item.input);
      }
      rows.push({ case: item.case, input: item.input, output });
    } finally { clock.restore(); }
  }
  const out = path.join(destination, "functions");
  await mkdir(out, { recursive: true });
  await writeFile(path.join(out, `${input.value.id}.jsonl`), rows.map(row => JSON.stringify(row,
    (_key, child) => child instanceof Set ? [...child] : child)).join("\n") + "\n");
}
