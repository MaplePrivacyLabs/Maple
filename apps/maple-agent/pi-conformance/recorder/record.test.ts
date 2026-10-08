import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { expect, test } from "vitest";
import type { AssistantMessage, Tool, ToolCall } from "@earendil-works/pi-ai";
import { deterministicEnvironment } from "./determinism.ts";
import { loadInputs, validateInput, type FunctionMatrix, type Input, type Json, type Scenario } from "./schema.ts";

/** Snapshot on delivery: upstream stream objects continue mutating afterwards. */
function snapshot(value: unknown): unknown {
  return JSON.parse(JSON.stringify(value, (_key, child) => {
    if (typeof child === "function" || child instanceof AbortSignal) return undefined;
    if (child instanceof Set) return [...child];
    return child;
  }));
}

function requestSnapshot(call: number, model: unknown, context: unknown, options: unknown) {
  const source = (options ?? {}) as Record<string, unknown>;
  return snapshot({ call, model, context, options: {
    ...source, signal: undefined,
    apiKey: typeof source.apiKey === "string" ? "<apiKey>" : source.apiKey,
  } });
}

const scenarioRoot = process.env.PI_SCENARIOS_DIR;
const outputRoot = process.env.PI_CORPUS_OUT;
if (!scenarioRoot || !outputRoot) throw new Error("Recorder output and scenario roots are required");
const inputs = await loadInputs(scenarioRoot);
const jsonl = (rows: unknown[]) => rows.map((row) => JSON.stringify(row)).join("\n") + "\n";

// These tests protect the cross-language input boundary, not Pi implementation details.
test("rejects malformed DSL before starting Pi", () => {
  const original = inputs.scenarios[0].value;
  for (const [description, change] of [
    ["unknown property", { unknown: true }],
    ["path traversal", { id: "agent/../../escape" }],
    ["two operations in one step", { steps: [{ prompt: "hello", abort: {} }] }],
    ["implicit tool call id", { provider: { kind: "faux", tokenSize: 4, responses: [
      { content: [{ type: "toolCall", name: "lookup", arguments: {} }], stopReason: "toolUse" },
    ] } }],
  ] as const) {
    expect(() => validateInput(inputs.validator, { ...original, ...change }, description)).toThrow();
  }
  expect(() => validateInput(inputs.validator, original, "valid input")).not.toThrow();
  if (inputs.functions.length > 0) {
    const matrix = inputs.functions[0].value;
    expect(() => validateInput(inputs.validator, {
      ...matrix, cases: [matrix.cases[0], matrix.cases[0]],
    }, "duplicate function cases")).toThrow(/Duplicate function case/);
    expect(() => validateInput(inputs.validator, {
      ...matrix, cases: [{ ...matrix.cases[0], input: { ...matrix.cases[0].input, unknown: true } }],
    }, "unknown function input")).toThrow(/Invalid DSL/);
  }
});

function assertSupportedScenario(scenario: Scenario) {
  if (scenario.layer !== "agent" || scenario.provider.kind !== "faux" || scenario.model.ref !== "faux-default") {
    throw new Error(`Recorder does not yet implement scenario capabilities: ${scenario.id}`);
  }
  if (scenario.tools?.length || scenario.variants?.length || scenario.provider.tokensPerSecond !== undefined) {
    throw new Error(`Recorder does not yet implement tools, subscriber variants or paced faux responses: ${scenario.id}`);
  }
  for (const step of scenario.steps) {
    const operation = Object.keys(step)[0];
    if (!["prompt", "advanceClock", "awaitIdle"].includes(operation)) {
      throw new Error(`Recorder does not yet implement ${operation}: ${scenario.id}`);
    }
    if (operation === "prompt" && typeof step.prompt !== "string") {
      throw new Error(`Recorder does not yet implement message-valued prompts: ${scenario.id}`);
    }
    if (operation === "awaitIdle" && (step.awaitIdle as Record<string, Json>).autoAdvance === true) {
      throw new Error(`Recorder does not yet implement awaitIdle autoAdvance: ${scenario.id}`);
    }
  }
}

for (const input of inputs.scenarios) {
  test(`records ${input.value.id} from pinned source`, async () => {
    const { value: scenario, source } = input;
    assertSupportedScenario(scenario);
    if (scenario.provider.kind !== "faux") throw new Error("Expected faux provider");
    const clock = deterministicEnvironment(scenario.clock.epochMs);
    try {
      const { Agent } = await import("@earendil-works/pi-agent-core");
      const { createFauxCore, fauxAssistantMessage } = await import("@earendil-works/pi-ai/providers/faux");
      const events: { seq: number; type: string; entries: number; data: unknown }[] = [];
      const requests: unknown[] = [];
      const emit = (type: string, data: unknown) => events.push({
        seq: events.length, type, entries: 0, data: snapshot(data),
      });
      const faux = createFauxCore({
        api: "faux", provider: "faux",
        tokenSize: { min: scenario.provider.tokenSize, max: scenario.provider.tokenSize },
      });
      faux.setResponses(scenario.provider.responses.map((response) =>
        (context, options, _state, model) => {
          requests.push(requestSnapshot(requests.length, model, context, options));
          return fauxAssistantMessage(response.content as AssistantMessage["content"] | string, {
            stopReason: response.stopReason,
            ...(response.errorMessage === undefined ? {} : { errorMessage: response.errorMessage }),
            ...(response.responseId === undefined ? {} : { responseId: response.responseId }),
            ...(response.timestamp === undefined ? {} : { timestamp: response.timestamp }),
          });
        }));
      const agent = new Agent({
        initialState: {
          model: faux.getModel(), systemPrompt: scenario.systemPrompt,
          thinkingLevel: scenario.thinkingLevel,
        },
        streamFn: faux.streamSimple,
        steeringMode: scenario.steeringMode,
        followUpMode: scenario.followUpMode,
        transformContext: async (messages) => {
          emit("$hook", { name: "transformContext", messages });
          return messages;
        },
        convertToLlm: (messages) => {
          emit("$hook", { name: "convertToLlm", messages });
          return messages;
        },
      });
      agent.subscribe((event) => {
        if (event.type === "message_update") {
          const { partial, ...delta } = event.assistantMessageEvent as
            typeof event.assistantMessageEvent & { partial?: AssistantMessage };
          emit(event.type, {
            message: event.message,
            assistantMessageEvent: delta,
            ...(partial && "contentIndex" in delta
              ? { block: partial.content[delta.contentIndex] } : {}),
          });
        } else {
          const { type, ...data } = event;
          emit(type, data);
        }
      });
      let pending: Promise<void> | undefined;
      for (const step of scenario.steps) {
        emit("$step", step);
        if ("prompt" in step) {
          if (agent.state.isStreaming) throw new Error(`Prompt while already active in ${scenario.id}`);
          pending = agent.prompt(step.prompt as string);
        } else if ("advanceClock" in step) {
          await clock.advance(step.advanceClock as number);
        } else if ("awaitIdle" in step) {
          await pending;
          await agent.waitForIdle();
        }
      }
      await pending;
      await agent.waitForIdle();
      expect(agent.state.isStreaming).toBe(false);
      expect(faux.getPendingResponseCount()).toBe(0);
      if (scenario.id === "agent/basic-text-turn") {
        expect(requests).toHaveLength(1);
        expect(agent.state.messages.at(-1)).toMatchObject({
          role: "assistant", content: [{ type: "text", text: "Hello, world!" }],
          stopReason: "stop", thinkingLevel: "off",
        });
        const hookNames = events.filter((event) => event.type === "$hook")
          .map((event) => (event.data as { name: string }).name);
        expect(hookNames).toEqual(["transformContext", "convertToLlm"]);
      }
      const out = path.join(outputRoot, "scenarios", scenario.id);
      await mkdir(out, { recursive: true });
      await writeFile(path.join(out, "scenario.json"), source);
      await writeFile(path.join(out, "events.jsonl"), jsonl(events));
      await writeFile(path.join(out, "requests.jsonl"), jsonl(requests));
      await writeFile(path.join(out, "final.json"), JSON.stringify(snapshot({
        state: agent.state, queues: { hasQueuedMessages: agent.hasQueuedMessages(), next: agent.peekQueuedMessages() },
        errors: [],
      }), null, 2) + "\n");
    } finally {
      clock.restore();
    }
  });
}

async function recordFunction(input: Input<FunctionMatrix>) {
  const rows: unknown[] = [];
  for (const item of input.value.cases) {
    const clock = deterministicEnvironment(input.value.clock.epochMs);
    let output: unknown;
    let error: string | undefined;
    try {
      const value = item.input;
      switch (input.value.id) {
        case "json-parse.parseStreamingJson": {
          const { parseStreamingJson } = await import("@earendil-works/pi-ai/utils/json-parse");
          output = parseStreamingJson(value.partialJson as string);
          break;
        }
        case "estimate.estimateTextTokens": {
          const { estimateTextTokens } = await import("@earendil-works/pi-ai/utils/estimate");
          output = estimateTextTokens(value.text as string);
          break;
        }
        case "sanitize-unicode.sanitizeSurrogates": {
          const { sanitizeSurrogates } = await import("@earendil-works/pi-ai/utils/sanitize-unicode");
          output = sanitizeSurrogates(String.fromCharCode(...value.utf16 as number[]));
          break;
        }
        case "json.stringify":
          output = JSON.stringify("utf16" in value ? String.fromCharCode(...value.utf16 as number[])
            : "entries" in value ? Object.fromEntries(value.entries as [string, Json][])
            : "number" in value ? Number(value.number)
            : value.value, null, value.space as string | number | undefined);
          break;
        case "validation.validateToolArguments": {
          const { validateToolArguments } = await import("@earendil-works/pi-ai/utils/validation");
          try {
            output = validateToolArguments(value.tool as unknown as Tool, value.toolCall as unknown as ToolCall);
          } catch (thrown) {
            error = thrown instanceof Error ? thrown.message : String(thrown);
          }
          break;
        }
        case "retry.retryDelayMs": {
          const { retryDelayMs } = await import("@earendil-works/pi-ai/utils/retry");
          output = retryDelayMs(value.policy as { baseDelayMs: number; maxAgentDelayMs?: number }, value.attempt as number);
          break;
        }
        case "env.virtualTimers": {
          const events: { id: string; nowMs: number }[] = [];
          type Timer = { id: string; delayMs: number; microtasksBeforeNext?: number; next?: Timer };
          const schedule = (timer: Timer) => setTimeout(async () => {
            events.push({ id: timer.id, nowMs: Date.now() });
            for (let index = 0; index < (timer.microtasksBeforeNext ?? 0); index++) await Promise.resolve();
            if (timer.next) schedule(timer.next);
          }, timer.delayMs);
          for (const operation of value.operations as Record<string, Json>[]) {
            if ("schedule" in operation) {
              schedule(operation.schedule as Timer);
            } else if ("advance" in operation) {
              await clock.advance(operation.advance as number);
            } else if ("setNow" in operation) {
              clock.setNow(operation.setNow as number);
            }
          }
          output = { events, nowMs: Date.now(), pendingTimers: clock.pendingTimers() };
          break;
        }
        default:
          throw new Error(`Recorder does not implement function ${input.value.id}`);
      }
      // Loader/import failures fail the recording; only an explicitly invoked
      // function's expected exception is eligible to become a golden error row.
      rows.push(error === undefined
        ? { case: item.case, input: item.input, output: snapshot(output) }
        : { case: item.case, input: item.input, error });
    } finally {
      clock.restore();
    }
  }
  const out = path.join(outputRoot, "functions");
  await mkdir(out, { recursive: true });
  await writeFile(path.join(out, `${input.value.id}.jsonl`), jsonl(rows));
}

for (const input of inputs.functions) {
  test(`records function ${input.value.id} from pinned source`, () => recordFunction(input));
}


for (const input of inputs.models) {
  test(`freezes selected catalog models ${input.value.id} from pinned source`, async () => {
    const clock = deterministicEnvironment(input.value.clock.epochMs);
    try {
      const { getModel } = await import("@earendil-works/pi-ai/compat");
      const models: Record<string, unknown> = {};
      for (const reference of input.value.models) {
        const model = getModel(reference.provider as never, reference.id as never);
        if (!model) throw new Error(`Pinned catalog lacks ${reference.provider}/${reference.id}`);
        models[`${reference.provider}/${reference.id}`] = snapshot(model);
      }
      const out = path.join(outputRoot, "models");
      await mkdir(out, { recursive: true });
      await writeFile(path.join(out, `${input.value.id}.json`), JSON.stringify(models, null, 2) + "\n");
    } finally {
      clock.restore();
    }
  });
}
