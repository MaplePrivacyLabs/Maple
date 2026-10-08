// Place beside record.test.ts in the frozen coding-agent test/recorder directory.
// Calls selected upstream functions only; contains input restoration and observation,
// never a reimplementation or an expected result.
import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { Usage } from "@earendil-works/pi-ai";
import {
  SessionManager, assertValidSessionId, buildContextEntries, buildSessionContext,
  buildSessionProjection, getLatestCompactionEntry, migrateSessionEntries,
  parseSessionEntries, sessionEntryToContextMessages,
  type FileEntry, type SessionEntry, type NewSessionOptions,
} from "../../src/core/session-manager.ts";
import {
  bashExecutionToText, convertToLlm, createBranchSummaryMessage,
  createCompactionSummaryMessage, createCustomMessage,
  type BashExecutionMessage,
} from "../../src/core/messages.ts";
import { serializeSessionBranch } from "../../src/core/session-export.ts";
import { createUsageTotals, addUsageToTotals, combineUsage, getUsageCostBreakdown } from "../../src/core/usage-totals.ts";
import { formatMissingSessionCwdError, formatMissingSessionCwdPrompt, MissingSessionCwdError, type SessionCwdIssue } from "../../src/core/session-cwd.ts";
import { splitBom, stripBom } from "../../src/utils/text.ts";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Input = Record<string, Json>;
type Observation = { value: unknown } | { error: string; errorClass: string };
class FixtureError extends Error {}
export const SESSION_FUNCTION_IDS = [
  "session.parseAndMigrate", "session.project", "session-manager.buildSessionContext", "session.entryToMessages",
  "session.assertValidId", "session.inMemory", "messages.convertToLlm",
  "messages.construct", "messages.bashExecutionToText", "usage.totals",
  "session.cwdFormatting", "text.bom",
] as const;
function invoke(call: () => unknown): Observation {
  try { return { value: call() }; }
  catch (error) {
    if (error instanceof FixtureError) throw error;
    return { error: error instanceof Error ? error.message : String(error), errorClass: error instanceof Error ? error.name : typeof error };
  }
}
function raw<T>(input: Input, field: string): T {
  const encoded = input[`${field}Json`];
  return (typeof encoded === "string" ? JSON.parse(encoded) : structuredClone(input[field])) as T;
}
function snapshot(value: unknown): unknown { return value === undefined ? undefined : JSON.parse(JSON.stringify(value)); }
function observeManager(manager: SessionManager) {
  return {
    header: manager.getHeader(), entries: manager.getEntries(), leafId: manager.getLeafId(),
    sessionId: manager.getSessionId(), persisted: manager.isPersisted(), sessionFile: manager.getSessionFile(),
    name: manager.getSessionName(), count: manager.getEntryCount(), branch: manager.getBranch(),
    tree: manager.getTree(), projection: manager.buildSessionProjection(),
    serialized: [manager.getHeader(), ...manager.getEntries()].map((entry) => JSON.stringify(entry)).join("\n") + "\n",
  };
}
function memory(input: Input): unknown {
  const manager = SessionManager.inMemory(input.cwd as string, input.options as NewSessionOptions | undefined,
    input.entries !== undefined || input.entriesJson !== undefined ? raw<FileEntry[]>(input, "entries") : undefined);
  const aliases = new Map<string, string>();
  const target = (value: Json | undefined): string => {
    if (typeof value !== "string") throw new FixtureError("Expected target string");
    return aliases.get(value) ?? value;
  };
  const results: unknown[] = [];
  for (const operation of input.operations as Input[]) {
    const result = invoke(() => {
      switch (operation.operation) {
        case "appendMessage": return manager.appendMessage(raw<Parameters<SessionManager["appendMessage"]>[0]>(operation, "message"));
        case "appendThinkingLevelChange": return manager.appendThinkingLevelChange(operation.thinkingLevel as string);
        case "appendModelChange": return manager.appendModelChange(operation.provider as string, operation.modelId as string);
        case "appendCustomEntry": return manager.appendCustomEntry(operation.customType as string, operation.data);
        case "appendCustomMessageEntry": return manager.appendCustomMessageEntry(operation.customType as string, operation.content as Parameters<SessionManager["appendCustomMessageEntry"]>[1], operation.display as boolean, operation.details);
        case "appendContextEdit": return manager.appendContextEdit(target(operation.target), operation.replacement as Parameters<SessionManager["appendContextEdit"]>[1]);
        case "appendLabelChange": return manager.appendLabelChange(target(operation.target), operation.label as string | undefined);
        case "appendSessionInfo": return manager.appendSessionInfo(operation.name as string);
        case "appendUsage": return manager.appendUsage(operation.kind as string, operation.provider as string, operation.model as string, operation.usage as unknown as Usage, operation.note as string | undefined);
        case "appendCompaction": return manager.appendCompaction(operation.summary as string, operation.firstKept == null ? null : target(operation.firstKept), operation.tokensBefore as number, operation.details, operation.fromHook as boolean | undefined, operation.usage as unknown as Usage | undefined);
        case "branchWithSummary": return manager.branchWithSummary(operation.branchFrom == null ? null : target(operation.branchFrom), operation.summary as string, operation.details, operation.fromHook as boolean | undefined, operation.usage as unknown as Usage | undefined);
        case "branch": return manager.branch(target(operation.target));
        case "resetLeaf": return manager.resetLeaf();
        case "createBranchedSession": return manager.createBranchedSession(target(operation.target));
        case "newSession": return manager.newSession(operation.options as NewSessionOptions | undefined);
        case "mutateEntry": {
          const entry = manager.getEntry(target(operation.target));
          if (!entry) throw new FixtureError("Mutation fixture has no entry");
          Object.assign(entry, operation.fields); return entry;
        }
        case "mutateMessage": {
          const entry = manager.getEntry(target(operation.target));
          if (!entry || entry.type !== "message") throw new FixtureError("Mutation fixture has no message");
          Object.assign(entry.message, operation.fields); return entry.message;
        }
        case "serializeBranch": {
          const callbackCalls: unknown[] = [];
          const jsonl = serializeSessionBranch(manager, operation.trailing ? (parentId, timestamp) => {
            callbackCalls.push({ parentId, timestamp });
            return (operation.trailing as Input[]).map((entry) => ({ ...entry, parentId, timestamp }));
          } : undefined);
          return { jsonl, callbackCalls };
        }
        default: throw new FixtureError(`Unknown session operation ${operation.operation}`);
      }
    });
    if (typeof operation.alias === "string" && "value" in result) {
      const id = typeof result.value === "string" ? result.value : (result.value as { id?: string } | undefined)?.id;
      if (id === undefined) throw new FixtureError("Alias operation returned no ID");
      aliases.set(operation.alias, id);
    }
    results.push(snapshot({ result, state: observeManager(manager) }));
  }
  return { results, aliases: Object.fromEntries(aliases) };
}
export function dispatchSessionFunction(id: string, input: Input): Observation {
  switch (id) {
    case "session.parseAndMigrate": {
      const content = input.content as string;
      return invoke(() => {
        const entries = parseSessionEntries(content);
        const before = entries.map((entry) => JSON.stringify(entry));
        if (input.migrate) migrateSessionEntries(entries);
        return { before, entries, serialized: entries.map((entry) => JSON.stringify(entry)).join("\n") + "\n" };
      });
    }
    case "session-manager.buildSessionContext": {
      const entries = raw<SessionEntry[]>(input, "entries");
      return invoke(() => buildSessionContext(entries, input.leafId as string | null | undefined));
    }
    case "session.project": {
      const entries = raw<SessionEntry[]>(input, "entries");
      return invoke(() => {
        const leaf = input.leafId as string | null | undefined;
        const projected = buildSessionProjection(entries, leaf);
        return { contextEntries: buildContextEntries(entries, leaf), projection: projected,
          context: buildSessionContext(entries, leaf), latestCompaction: getLatestCompactionEntry(entries),
          sourceIndices: projected.entries.map((entry) => entries.indexOf(entry.sourceEntry)),
          originalMessages: projected.messages.map((message) => entries.findIndex((entry) => entry.type === "message" && entry.message === message)),
          rawEntriesAfter: entries.map((entry) => JSON.stringify(entry)) };
      });
    }
    case "session.entryToMessages": {
      const entry = raw<SessionEntry>(input, "entry");
      return invoke(() => ({ messages: sessionEntryToContextMessages(entry), rawEntryAfter: JSON.stringify(entry) }));
    }
    case "session.assertValidId": return invoke(() => assertValidSessionId(input.id as string));
    case "session.inMemory": return invoke(() => memory(input));
    case "messages.convertToLlm": {
      const messages = raw<AgentMessage[]>(input, "messages");
      return invoke(() => { const output = convertToLlm(messages); return { messages: output, inputIndices: output.map((message) => messages.indexOf(message)), rawInputsAfter: messages.map((message) => JSON.stringify(message)), serialized: output.map((message) => JSON.stringify(message)) }; });
    }
    case "messages.bashExecutionToText": return invoke(() => bashExecutionToText(raw<BashExecutionMessage>(input, "message")));
    case "messages.construct": return invoke(() => {
      switch (input.operation) {
        case "branch": return createBranchSummaryMessage(input.summary as string, input.fromId as string, input.timestamp as string);
        case "compaction": return createCompactionSummaryMessage(input.summary as string, input.tokensBefore as number, input.timestamp as string);
        case "custom": return createCustomMessage(input.customType as string, input.content as Parameters<typeof createCustomMessage>[1], input.display as boolean, input.details, input.timestamp as string);
        default: throw new FixtureError("Unknown message construction operation");
      }
    });
    case "usage.totals": return invoke(() => {
      const usages = input.usages as unknown as Usage[];
      const totals = createUsageTotals();
      for (const usage of usages) addUsageToTotals(totals, usage);
      return { totals, combined: usages.length === 2 ? combineUsage(usages[0], usages[1]) : undefined,
        breakdown: getUsageCostBreakdown(raw<SessionEntry[]>(input, "entries")) };
    });
    case "session.cwdFormatting": return invoke(() => {
      const issue = input.issue as unknown as SessionCwdIssue; const error = new MissingSessionCwdError(issue);
      return { error: formatMissingSessionCwdError(issue), prompt: formatMissingSessionCwdPrompt(issue), exception: { name: error.name, message: error.message, issue: error.issue } };
    });
    case "text.bom": return invoke(() => ({ split: splitBom(input.text as string), stripped: stripBom(input.text as string) }));
    default: throw new FixtureError(`Unsupported session function ${id}`);
  }
}
