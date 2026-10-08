// Copy beside record.test.ts at packages/coding-agent/test/maple-recorder/.
// Imports resolve directly to the frozen selected source under the existing
// coding-agent Vitest configuration. No implementation or expected output lives here.
import type { AgentMessage, StreamFn, ThinkingLevel } from "@earendil-works/pi-agent-core";
import {
  createAssistantMessageEventStream,
  normalizeContext,
  type AssistantMessage,
  type AssistantMessageEvent,
  type Context,
  type Message,
  type Model,
  type RetryPolicy,
  type SimpleStreamOptions,
  type Usage,
} from "@earendil-works/pi-ai";
import {
  calculateContextTokens,
  compact,
  completeSummarization,
  estimateContextTokens,
  estimateProjectedContextTokens,
  estimateTokens,
  findCutPoint,
  generateSummary,
  generateSummaryWithUsage,
  getLastAssistantUsage,
  prepareCompaction,
  shouldCompact,
  type CompactionPreparation,
  type CompactionSettings,
} from "../../src/core/compaction/compaction.ts";
import {
  collectEntriesForBranchSummary,
  generateBranchSummary,
  prepareBranchEntries,
} from "../../src/core/compaction/branch-summarization.ts";
import {
  computeFileLists,
  createFileOps,
  extractFileOpsFromMessage,
  formatFileOperations,
  serializeConversation,
  type FileOperations,
} from "../../src/core/compaction/utils.ts";
import {
  buildSessionProjection,
  type ReadonlySessionManager,
  type SessionEntry,
} from "../../src/core/session-manager.ts";

export const COMPACTION_FUNCTION_IDS = [
  "compaction.calculateContextTokens",
  "compaction.getLastAssistantUsage",
  "compaction.estimateContextTokens",
  "compaction.estimateProjectedContextTokens",
  "compaction.estimateTokens",
  "compaction.shouldCompact",
  "compaction.findCutPoint",
  "compaction.prepareCompaction",
  "compaction-utils.serializeConversation",
  "compaction.serializeConversation",
  "compaction-utils.fileOperations",
  "branch-summarization.prepareBranchEntries",
  "branch-summarization.collectEntriesForBranchSummary",
  "compaction.summarization",
  "compaction.retryRequestOwnership",
  "branch-summarization.generateBranchSummary",
] as const;

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Input = Record<string, Json>;
type DoneReason = Extract<AssistantMessageEvent, { type: "done" }>["reason"];
type InvocationResult = { value: unknown } | { error: string; errorClass: string };

interface SummaryOptionsInput {
  apiKey?: string;
  headers?: Record<string, string>;
  env?: Record<string, string>;
  customInstructions?: string;
  previousSummary?: string;
  thinkingLevel?: ThinkingLevel;
  sessionId?: string;
  retry?: RetryPolicy;
  cacheRetention?: SimpleStreamOptions["cacheRetention"];
  toolChoice?: SimpleStreamOptions["toolChoice"];
}

class FixtureError extends Error {}

function snapshot(value: unknown): unknown {
  if (value === undefined) return undefined;
  return JSON.parse(JSON.stringify(value, (_key, child: unknown) => {
    if (typeof child === "function" || child instanceof AbortSignal) return undefined;
    if (child instanceof Set) return [...child];
    return child;
  }));
}

/** A raw JSON string keeps lone UTF-16 surrogates out of foreign JSON decoders. */
function messages<T extends AgentMessage | Message>(input: Input): T[] {
  if (typeof input.messagesJson === "string") return JSON.parse(input.messagesJson) as T[];
  return structuredClone(input.messages) as unknown as T[];
}

function restoreFileOps(input: unknown): FileOperations {
  const value = input as { read: string[]; written: string[]; edited: string[] };
  return { read: new Set(value.read), written: new Set(value.written), edited: new Set(value.edited) };
}

function scriptedStream(input: Input) {
  const responses = structuredClone(input.responses) as unknown as AssistantMessage[];
  const reasons = structuredClone(input.doneReasons) as DoneReason[];
  if (responses.length !== reasons.length) throw new FixtureError("Response/done-reason counts differ");
  const requests: unknown[] = [];
  const streamFn: StreamFn = (model, context, options) => {
    const index = requests.length;
    const response = responses[index];
    const reason = reasons[index];
    if (!response || !reason) throw new FixtureError("Compaction scripted responses exhausted");
    requests.push(snapshot({ model, context, options }));
    const stream = createAssistantMessageEventStream();
    queueMicrotask(() => stream.push({ type: "done", reason, message: response }));
    return stream;
  };
  return { requests, streamFn };
}

/** Only exceptions from the selected invocation become source-error observations. */
async function invoke(call: () => unknown | Promise<unknown>): Promise<InvocationResult> {
  try {
    return { value: await call() };
  } catch (error) {
    if (error instanceof FixtureError) throw error;
    return { error: error instanceof Error ? error.message : String(error),
      errorClass: error instanceof Error ? error.name : typeof error };
  }
}

async function runSummarization(input: Input): Promise<unknown> {
  const { requests, streamFn } = scriptedStream(input);
  const model = structuredClone(input.model) as unknown as Model;
  const options = structuredClone(input.options) as unknown as SummaryOptionsInput;
  const currentMessages = messages<AgentMessage>(input);
  const results: InvocationResult[] = [];
  const times = input.times === undefined ? 1 : input.times as number;
  if (!Number.isSafeInteger(times) || times < 1) throw new FixtureError("Invalid invocation count");
  const operation = input.operation;
  if (!["generateSummary", "generateSummaryWithUsage", "compact", "completeSummarization"].includes(operation as string)) {
    throw new FixtureError(`Unsupported compaction operation ${operation}`);
  }
  // Input restoration happens outside the captured selected-source invocation.
  const preparation = input.preparation === undefined ? undefined
    : structuredClone(input.preparation) as unknown as CompactionPreparation;
  if (preparation) preparation.fileOps = restoreFileOps(preparation.fileOps);
  const context = input.context === undefined ? undefined
    : normalizeContext(structuredClone(input.context) as unknown as Context);
  for (let index = 0; index < times; index++) {
    results.push(await invoke(() => {
      switch (operation) {
        case "generateSummary":
          return generateSummary(currentMessages, model, input.reserveTokens as number, options.apiKey,
            options.headers, undefined, options.customInstructions, options.previousSummary, options.thinkingLevel,
            streamFn, options.env, options.retry, undefined, options.sessionId);
        case "generateSummaryWithUsage":
          return generateSummaryWithUsage(currentMessages, model, input.reserveTokens as number, options.apiKey,
            options.headers, undefined, options.customInstructions, options.previousSummary, options.thinkingLevel,
            streamFn, options.env, options.retry, undefined, options.sessionId);
        case "compact":
          if (!preparation) throw new FixtureError("Missing compaction preparation");
          return compact(preparation, model, options.apiKey, options.headers, options.customInstructions,
            undefined, options.thinkingLevel, streamFn, options.env, options.retry, undefined, options.sessionId);
        case "completeSummarization":
          if (!context) throw new FixtureError("Missing completeSummarization context");
          return completeSummarization(model, context, options, streamFn, options.retry);
        default:
          throw new FixtureError("Unsupported compaction operation");
      }
    }));
  }
  return { results, requests };
}

/** Observe this ordinary request DTO ownership boundary independently of mutable hooks. */
async function retryRequestOwnership(input: Input): Promise<unknown> {
  const model = structuredClone(input.model) as unknown as Model;
  const context = normalizeContext(structuredClone(input.context) as unknown as Context);
  const options = structuredClone(input.options) as unknown as SimpleStreamOptions;
  options.fetch = globalThis.fetch;
  const changes = input.mutations as Record<string, Json>;
  const headerName = changes.headerName as string;
  const responses = structuredClone(input.responses) as unknown as AssistantMessage[];
  const calls: unknown[] = [];
  const requestSnapshot = (requestModel: Model, requestContext: typeof context, requestOptions: SimpleStreamOptions) => ({
    modelName: requestModel.name, messageCount: requestContext.messages.length,
    maxTokens: requestOptions.maxTokens, header: requestOptions.headers?.[headerName],
  });
  const streamFn: StreamFn = (requestModel, requestContext, requestOptions) => {
    if (!requestOptions) throw new FixtureError("Retry ownership fixture requires request options");
    const index = calls.length;
    calls.push(requestSnapshot(requestModel, requestContext, requestOptions));
    if (index === 0) {
      requestModel.name = changes.modelName as string;
      requestContext.messages.push(structuredClone(changes.appendMessage) as unknown as Message);
      requestOptions.maxTokens = changes.maxTokens as number;
      if (!requestOptions.headers) throw new FixtureError("Retry ownership fixture requires headers");
      requestOptions.headers[headerName] = changes.headerValue as string;
    }
    const response = responses[index];
    if (!response) throw new FixtureError("Retry ownership responses exhausted");
    const stream = createAssistantMessageEventStream();
    if (response.stopReason === "error" || response.stopReason === "aborted")
      stream.push({ type: "error", reason: response.stopReason, error: response });
    else stream.push({ type: "done", reason: response.stopReason, message: response });
    return stream;
  };
  const response = await completeSummarization(model, context, options, streamFn, input.retry as unknown as RetryPolicy);
  return { calls, callerAfter: requestSnapshot(model, context, options), stopReason: response.stopReason };
}

/**
 * Returns an observation wrapper. `value: undefined` disappears on snapshot,
 * preserving absence as {} without conflating source undefined with null.
 * Input validation/import errors must be allowed to fail the recorder.
 */
export async function dispatchCompactionFunction(id: string, input: Input): Promise<InvocationResult> {
  switch (id) {
    case "compaction.calculateContextTokens":
      return invoke(() => calculateContextTokens(input.usage as unknown as Usage));
    case "compaction.getLastAssistantUsage":
      return invoke(() => getLastAssistantUsage(input.entries as unknown as SessionEntry[]));
    case "compaction.estimateContextTokens": {
      const data = messages<AgentMessage>(input);
      return invoke(() => estimateContextTokens(data));
    }
    case "compaction.estimateTokens":
      return invoke(() => estimateTokens(input.message as unknown as AgentMessage));
    case "compaction.shouldCompact":
      return invoke(() => shouldCompact(input.contextTokens as number, input.contextWindow as number,
        input.settings as unknown as CompactionSettings));
    case "compaction.findCutPoint":
      return invoke(() => findCutPoint(input.entries as unknown as SessionEntry[], input.startIndex as number,
        input.endIndex as number, input.keepRecentTokens as number));
    case "compaction.prepareCompaction":
      return invoke(() => prepareCompaction(input.entries as unknown as SessionEntry[], input.settings as unknown as CompactionSettings));
    case "compaction.estimateProjectedContextTokens": {
      const entries = input.entries as unknown as SessionEntry[];
      return invoke(() => estimateProjectedContextTokens(buildSessionProjection(entries), entries));
    }
    case "compaction.serializeConversation":
    case "compaction-utils.serializeConversation": {
      const data = messages<Message>(input);
      return invoke(() => serializeConversation(data));
    }
    case "compaction-utils.fileOperations": {
      const data = messages<AgentMessage>(input);
      return invoke(() => {
        const fileOps = createFileOps();
        for (const message of data) extractFileOpsFromMessage(message, fileOps);
        const lists = computeFileLists(fileOps);
        return { fileOps, ...lists, formatted: formatFileOperations(lists.readFiles, lists.modifiedFiles) };
      });
    }
    case "branch-summarization.prepareBranchEntries":
      return invoke(() => prepareBranchEntries(input.entries as unknown as SessionEntry[], input.tokenBudget as number));
    case "branch-summarization.collectEntriesForBranchSummary": {
      const entries = structuredClone(input.entriesById) as unknown as Record<string, SessionEntry>;
      const branches = structuredClone(input.branches) as unknown as Record<string, SessionEntry[]>;
      const session = {
        getEntry: (key: string) => entries[key],
        getBranch: (key?: string) => branches[key ?? ""] ?? [],
      } as unknown as ReadonlySessionManager;
      return invoke(() => collectEntriesForBranchSummary(session, input.oldLeafId as string | null, input.targetId as string));
    }
    case "compaction.summarization":
      return { value: await runSummarization(input) };
    case "compaction.retryRequestOwnership":
      return invoke(() => retryRequestOwnership(input));
    case "branch-summarization.generateBranchSummary": {
      const { requests, streamFn } = scriptedStream(input);
      const options = structuredClone(input.options) as unknown as Omit<Parameters<typeof generateBranchSummary>[1], "signal" | "streamFn">;
      const entries = structuredClone(input.entries) as unknown as SessionEntry[];
      const result = await invoke(() => generateBranchSummary(entries, {
        ...options, signal: new AbortController().signal, streamFn,
      }));
      return { value: { result, requests } };
    }
    default:
      throw new FixtureError(`Unsupported compaction function ${id}`);
  }
}
