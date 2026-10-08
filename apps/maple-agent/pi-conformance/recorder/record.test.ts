import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { expect, test } from "vitest";
import { deterministicEnvironment } from "./determinism.ts";

/** Snapshot on delivery: upstream stream objects continue mutating afterwards. */
function snapshot(value: unknown): any {
  return JSON.parse(JSON.stringify(value, (key, child) => {
    if (typeof child === "function" || key === "signal") return undefined;
    if (key === "apiKey" && typeof child === "string") return "<apiKey>";
    if (child instanceof Set) return [...child];
    return child;
  }));
}

test("records agent/basic-text-turn from pinned source", async () => {
  const scenarioRoot = process.env.PI_SCENARIOS_DIR;
  const outputRoot = process.env.PI_CORPUS_OUT;
  if (!scenarioRoot || !outputRoot) throw new Error("Recorder output and scenario roots are required");
  const source = await readFile(path.join(scenarioRoot, "agent/basic-text-turn.json"), "utf8");
  const scenario = JSON.parse(source);
  expect(scenario.dsl).toBe(1);
  expect(scenario.id).toBe("agent/basic-text-turn");
  const restore = deterministicEnvironment(scenario.clock.epochMs);
  try {
    const { Agent } = await import("@earendil-works/pi-agent-core");
    const { createFauxCore, fauxAssistantMessage } = await import("@earendil-works/pi-ai/providers/faux");
    const events: unknown[] = [];
    const requests: unknown[] = [];
    const emit = (type: string, data: unknown) => events.push(snapshot({
      seq: events.length, type, entries: 0, data,
    }));
    const faux = createFauxCore({
      api: "faux", provider: "faux",
      tokenSize: { min: scenario.provider.tokenSize, max: scenario.provider.tokenSize },
    });
    faux.setResponses(scenario.provider.responses.map((response: any) =>
      (context: unknown, options: unknown, _state: unknown, model: unknown) => {
        requests.push(snapshot({ call: requests.length, model, context, options }));
        return fauxAssistantMessage(response.content, { stopReason: response.stopReason });
      }));
    const agent = new Agent({
      initialState: {
        model: faux.getModel(), systemPrompt: scenario.systemPrompt,
        thinkingLevel: scenario.thinkingLevel,
      },
      streamFn: faux.streamSimple,
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
        const update: any = event.assistantMessageEvent;
        const { partial, ...delta } = update;
        emit(event.type, {
          message: event.message,
          assistantMessageEvent: delta,
          ...(partial && "contentIndex" in update
            ? { block: partial.content[update.contentIndex] } : {}),
        });
      } else {
        const { type, ...data } = event;
        emit(type, data);
      }
    });
    let pending: Promise<void> | undefined;
    for (const step of scenario.steps) {
      emit("$step", step);
      if ("prompt" in step) pending = agent.prompt(step.prompt);
      else if ("awaitIdle" in step) {
        await pending;
        await agent.waitForIdle();
      } else throw new Error(`Unsupported spike step: ${JSON.stringify(step)}`);
    }
    await pending;
    expect(requests).toHaveLength(1);
    expect(agent.state.messages.at(-1)).toMatchObject({
      role: "assistant", content: [{ type: "text", text: "Hello, world!" }],
      stopReason: "stop", thinkingLevel: "off",
    });
    const hookNames = events.filter((e: any) => e.type === "$hook").map((e: any) => e.data.name);
    expect(hookNames).toEqual(["transformContext", "convertToLlm"]);
    expect(agent.state.isStreaming).toBe(false);
    expect(faux.getPendingResponseCount()).toBe(0);
    const out = path.join(outputRoot, "scenarios", scenario.id);
    await mkdir(out, { recursive: true });
    const jsonl = (rows: unknown[]) => rows.map((row) => JSON.stringify(row)).join("\n") + "\n";
    await writeFile(path.join(out, "scenario.json"), source);
    await writeFile(path.join(out, "events.jsonl"), jsonl(events));
    await writeFile(path.join(out, "requests.jsonl"), jsonl(requests));
    await writeFile(path.join(out, "final.json"), JSON.stringify(snapshot({
      state: agent.state, queues: { hasQueuedMessages: agent.hasQueuedMessages(), next: agent.peekQueuedMessages() },
      errors: [],
    }), null, 2) + "\n");
  } finally {
    restore();
  }
});
