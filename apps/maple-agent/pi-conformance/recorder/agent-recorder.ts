import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { expect, vi } from "vitest";
import type { Agent, AgentMessage, AgentTool } from "@earendil-works/pi-agent-core";
import type { AssistantMessage, Model } from "@earendil-works/pi-ai";
import { deterministicEnvironment } from "./determinism.ts";
import type { Input, Json, Scenario } from "./schema.ts";

type ObjectValue = Record<string, any>;
type Selector = { type: string; toolCallId?: string; role?: string; name?: string; assistantMessageEventType?: string; count?: number };
type Action = Record<string, Json>;
type Rule = { when?: { call?: number; toolCallId?: string }; behavior: Action[] };
type HookName = "beforeToolCall" | "afterToolCall" | "prepareRequest" | "prepareNextTurn" | "finishTurn";
interface AgentScenario extends Scenario {
  activeTools?: string[];
  toolExecution?: "sequential" | "parallel";
  hooks?: Partial<Record<HookName, Rule[]>>;
  subscribers?: { when: Selector; wait: string }[];
}
interface RecordEvent { seq: number; type: string; entries: number; data: ObjectValue }

/** Snapshots are taken on delivery, including shared partial blocks as observed then. */
function snapshot<T>(value: T): T {
  return JSON.parse(JSON.stringify(value, (_key, child) => {
    if (typeof child === "function" || child instanceof AbortSignal) return undefined;
    return child instanceof Set ? [...child] : child;
  }));
}

function matches(event: RecordEvent, selector: Selector) {
  return event.type === selector.type
    && (selector.toolCallId === undefined || event.data.toolCallId === selector.toolCallId)
    && (selector.role === undefined || event.data.message?.role === selector.role)
    && (selector.name === undefined || event.data.name === selector.name)
    && (selector.assistantMessageEventType === undefined
      || event.data.assistantMessageEvent?.type === selector.assistantMessageEventType);
}

function interpolate(value: Json, bindings: ObjectValue): any {
  const lookup = (field: string) => {
    let current: any = bindings;
    for (const key of field.split(".")) {
      if (current === null || typeof current !== "object" || !Object.hasOwn(current, key)) {
        throw new Error(`Unknown script interpolation ${field}`);
      }
      current = current[key];
    }
    return snapshot(current);
  };
  if (typeof value === "string") {
    const whole = value.match(/^\$\{([a-zA-Z0-9_.]+)\}$/);
    if (whole) return lookup(whole[1]);
    return value.replace(/\$\{([a-zA-Z0-9_.]+)\}/g, (_match, field) => {
      const replacement = lookup(field);
      return typeof replacement === "string" ? replacement : JSON.stringify(replacement);
    });
  }
  if (Array.isArray(value)) return value.map((child) => interpolate(child, bindings));
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([key, child]) => [key, interpolate(child, bindings)]));
  }
  return value;
}

class Gates {
  private state = new Map<string, { open: boolean; resolve: () => void; promise: Promise<void> }>();
  private gate(name: string) {
    if (!this.state.has(name)) {
      let resolve!: () => void;
      const promise = new Promise<void>((done) => { resolve = done; });
      this.state.set(name, { open: false, resolve, promise });
    }
    return this.state.get(name)!;
  }
  open(name: string) { const gate = this.gate(name); gate.open = true; gate.resolve(); }
  async wait(name: string, signal?: AbortSignal) {
    if (signal?.aborted) throw new Error("Operation aborted");
    const gate = this.gate(name);
    if (gate.open) return;
    if (!signal) return gate.promise;
    let onAbort!: () => void;
    const aborted = new Promise<never>((_resolve, reject) => {
      onAbort = () => reject(new Error("Operation aborted"));
      signal.addEventListener("abort", onAbort, { once: true });
    });
    try { await Promise.race([gate.promise, aborted]); }
    finally { signal.removeEventListener("abort", onAbort); }
  }
  openAll() { for (const name of this.state.keys()) this.open(name); }
}

const settle = () => new Promise<void>((resolve) => setImmediate(resolve));
const jsonl = (rows: unknown[]) => rows.map((row) => JSON.stringify(row)).join("\n") + "\n";

export async function recordAgentScenario(input: Input<Scenario>, outputRoot: string) {
  const scenario = input.value as AgentScenario;
  if (scenario.layer !== "agent" || scenario.provider.kind !== "faux" || scenario.model.ref !== "faux-default") {
    throw new Error(`Agent recorder requires the faux agent layer: ${scenario.id}`);
  }
  if (scenario.variants?.length) throw new Error("Materialize variants as separate scenario inputs");
  const clock = deterministicEnvironment(scenario.clock.epochMs);
  const gates = new Gates();
  let agent: Agent | undefined;
  let pending: Promise<void> | undefined;
  try {
    const { Agent } = await import("@earendil-works/pi-agent-core");
    const { createFauxCore, fauxAssistantMessage } = await import("@earendil-works/pi-ai/providers/faux");
    const events: RecordEvent[] = [];
    const requests: unknown[] = [];
    const errors: { message: string }[] = [];
    const emit = (type: string, data: ObjectValue) => {
      events.push({ seq: events.length, type, entries: 0, data: snapshot(data) });
    };
    const faux = createFauxCore({
      api: "faux", provider: "faux",
      tokenSize: { min: scenario.provider.tokenSize, max: scenario.provider.tokenSize },
      tokensPerSecond: scenario.provider.tokensPerSecond,
    });
    faux.setResponses(scenario.provider.responses.map((response) => (context, options, _state, model) => {
      requests.push(snapshot({ call: requests.length, model, context, options: {
        ...options, signal: undefined,
        apiKey: typeof options?.apiKey === "string" ? "<apiKey>" : options?.apiKey,
      } }));
      return fauxAssistantMessage(response.content as AssistantMessage["content"] | string, {
        stopReason: response.stopReason,
        ...(response.errorMessage === undefined ? {} : { errorMessage: response.errorMessage }),
        ...(response.responseId === undefined ? {} : { responseId: response.responseId }),
        ...(response.timestamp === undefined ? {} : { timestamp: response.timestamp }),
      });
    }));
    async function actions(behavior: Action[], context: ObjectValue, signal?: AbortSignal, onUpdate?: (update: any) => void) {
      const bindings = { ...context.args, ...context };
      for (const action of behavior) {
        if ("wait" in action) await gates.wait(interpolate(action.wait, bindings), signal);
        else if ("update" in action) {
          if (!onUpdate) throw new Error("Updates require a tool invocation");
          onUpdate(interpolate(action.update, bindings));
        } else if ("mutateArgs" in action) Object.assign(context.args, interpolate(action.mutateArgs, bindings));
        else if ("abort" in action) agent!.abort();
        else if ("throw" in action) throw new Error(interpolate(action.throw, bindings));
        else if ("return" in action) return interpolate(action.return, bindings);
        else if ("result" in action) return interpolate(action.result, bindings);
        else throw new Error(`Unsupported agent script action ${Object.keys(action)[0]}`);
      }
      return undefined;
    }
    const allTools: AgentTool[] = (scenario.tools ?? []).map((definition: ObjectValue) => {
      const tool: AgentTool = {
        name: definition.name,
        label: definition.label ?? definition.name,
        description: definition.description ?? "",
        parameters: definition.parameters,
        ...(definition.executionMode === undefined ? {} : { executionMode: definition.executionMode }),
        execute: async (toolCallId, args, signal, onUpdate) => {
          emit("$tool_call", { toolCallId, toolName: definition.name, args });
          const result = await actions(definition.behavior, { args, toolCallId, toolName: definition.name }, signal, onUpdate);
          if (result === undefined) throw new Error(`Tool ${definition.name} has no scripted result`);
          return result;
        },
      };
      if (definition.prepareArguments) {
        tool.prepareArguments = (args) => {
          emit("$hook", { name: "prepareArguments", toolName: definition.name, args });
          const prepared = definition.prepareArguments;
          if ("return" in prepared) return interpolate(prepared.return, { args });
          return { ...(args as ObjectValue), ...interpolate(prepared.set, { args }) };
        };
      }
      return tool;
    });
    const toolsByNames = (names: string[]) => names.map((name) => {
      const tool = allTools.find((entry) => entry.name === name);
      if (!tool) throw new Error(`Unknown scripted tool ${name}`);
      return tool;
    });
    function updateState(value: any) {
      if (!value || typeof value !== "object") return value;
      if (value.context?.tools) value.context.tools = toolsByNames(value.context.tools);
      if (value.model?.ref) {
        if (value.model.ref !== "faux-default") throw new Error("Unknown scripted model reference");
        const { ref: _ref, ...overrides } = value.model;
        value.model = { ...faux.getModel(), ...overrides };
      }
      return value;
    }
    const hooks: ObjectValue = {};
    for (const [name, rules] of Object.entries(scenario.hooks ?? {})) {
      let calls = 0;
      const hook = async (context: ObjectValue, signal?: AbortSignal) => {
        calls++;
        emit("$hook", { name, call: calls, context });
        const rule = rules!.find((entry) => (entry.when?.call === undefined || entry.when.call === calls)
          && (entry.when?.toolCallId === undefined || entry.when.toolCallId === context.toolCall?.id));
        const result = rule ? await actions(rule.behavior, context, signal) : undefined;
        return name === "prepareRequest" || name === "prepareNextTurn" ? updateState(result) : result;
      };
      hooks[name === "prepareNextTurn" ? "prepareNextTurnWithContext" : name] = hook;
    }
    agent = new Agent({
      initialState: {
        model: faux.getModel(), systemPrompt: scenario.systemPrompt, thinkingLevel: scenario.thinkingLevel,
        tools: scenario.activeTools ? toolsByNames(scenario.activeTools) : allTools,
      },
      streamFn: faux.streamSimple,
      steeringMode: scenario.steeringMode, followUpMode: scenario.followUpMode,
      toolExecution: scenario.toolExecution,
      transformContext: async (messages) => { emit("$hook", { name: "transformContext", messages }); return messages; },
      convertToLlm: (messages) => { emit("$hook", { name: "convertToLlm", messages }); return messages; },
      ...hooks,
    });
    for (const subscriber of scenario.subscribers ?? []) {
      let seen = 0;
      agent.subscribe(async (event, signal) => {
        const { type, ...data } = event;
        if (!matches({ seq: 0, entries: 0, type, data }, subscriber.when)) return;
        seen++;
        if (seen !== (subscriber.when.count ?? 1)) return;
        emit("$hook", { name: "subscriber", event, gate: subscriber.wait });
        await gates.wait(subscriber.wait, signal);
      });
    }
    // The observer is last so it sees persistence/subscriber barriers at delivery.
    agent.subscribe((event) => {
      if (event.type === "message_update") {
        const { partial, ...delta } = event.assistantMessageEvent as typeof event.assistantMessageEvent & { partial?: AssistantMessage };
        emit(event.type, {
          message: event.message, assistantMessageEvent: delta,
          ...(partial && "contentIndex" in delta ? { block: partial.content[delta.contentIndex] } : {}),
        });
      } else { const { type, ...data } = event; emit(type, data); }
    });
    async function awaitIdle(autoAdvance: boolean) {
      for (let turns = 0; agent!.state.isStreaming; turns++) {
        if (turns > 10000) throw new Error("Scripted agent did not become idle");
        await settle();
        if (!agent!.state.isStreaming) break;
        if (autoAdvance && clock.pendingTimers() > 0) await vi.advanceTimersToNextTimerAsync();
        else throw new Error("awaitIdle is blocked; open scripted gates or explicitly advance the clock");
      }
      await pending;
      await agent!.waitForIdle();
    }
    async function waitFor(selector: Selector) {
      for (;;) {
        if (events.filter((event) => matches(event, selector)).length >= (selector.count ?? 1)) return;
        const previous = events.length;
        await settle();
        if (events.filter((event) => matches(event, selector)).length >= (selector.count ?? 1)) return;
        if (events.length === previous) throw new Error(`waitFor did not match ${JSON.stringify(selector)}; open a gate or advance time`);
      }
    }
    function step(value: Record<string, Json>, expectCompletion = false): void | Promise<void> {
      if ("prompt" in value || "continue" in value) {
        const next = "prompt" in value ? agent!.prompt(value.prompt as string) : agent!.continue();
        if (expectCompletion) return next;
        else pending = next;
      } else if ("steer" in value) agent!.steer(value.steer as unknown as AgentMessage);
      else if ("followUp" in value) agent!.followUp(value.followUp as unknown as AgentMessage);
      else if ("abort" in value) agent!.abort();
      else if ("open" in value) gates.open(value.open as string);
      else if ("waitFor" in value) return waitFor(value.waitFor as Selector);
      else if ("advanceClock" in value) return clock.advance(value.advanceClock as number);
      else if ("awaitIdle" in value) return awaitIdle((value.awaitIdle as ObjectValue).autoAdvance === true);
      else if ("setActiveTools" in value) agent!.state.tools = toolsByNames(value.setActiveTools as string[]);
      else if ("setThinkingLevel" in value) agent!.state.thinkingLevel = value.setThinkingLevel as any;
      else if ("setModel" in value) agent!.state.model = updateState({ model: value.setModel }).model as Model<any>;
      else if ("expectError" in value) return (async () => {
        const expectation = value.expectError as ObjectValue;
        let thrown: unknown;
        try { await step(expectation.step, true); } catch (error) { thrown = error; }
        if (thrown === undefined) throw new Error("Expected scripted API call to throw");
        const message = thrown instanceof Error ? thrown.message : String(thrown);
        expect(message).toBe(expectation.message);
        errors.push({ message });
        emit("$api_error", { message });
      })();
      else throw new Error(`Unsupported agent step ${Object.keys(value)[0]}`);
    }
    for (const instruction of scenario.steps) {
      emit("$step", instruction);
      const completion = step(instruction);
      if (completion) await completion;
    }
    await awaitIdle(false);
    expect(faux.getPendingResponseCount()).toBe(0);
    expect(faux.state.callCount).toBe(scenario.provider.responses.length);
    expect(requests).toHaveLength(scenario.provider.responses.length);
    if (scenario.id === "agent/basic-text-turn") {
      expect(agent.state.messages.at(-1)).toMatchObject({ role: "assistant", content: [{ type: "text", text: "Hello, world!" }], stopReason: "stop", thinkingLevel: "off" });
    }
    const out = path.join(outputRoot, "scenarios", scenario.id);
    await mkdir(out, { recursive: true });
    await writeFile(path.join(out, "scenario.json"), input.source);
    await writeFile(path.join(out, "events.jsonl"), jsonl(events));
    await writeFile(path.join(out, "requests.jsonl"), jsonl(requests));
    await writeFile(path.join(out, "final.json"), JSON.stringify(snapshot({
      state: agent.state,
      queues: { hasQueuedMessages: agent.hasQueuedMessages(), next: agent.peekQueuedMessages() },
      errors,
    }), null, 2) + "\n");
  } finally {
    agent?.abort();
    gates.openAll();
    if (pending) { await vi.runAllTimersAsync(); await pending.catch(() => {}); }
    clock.restore();
  }
}
