import "./network-guard.ts";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import assert from "node:assert/strict";
import { deterministicEnvironment } from "./determinism.ts";
import type { Input, Scenario } from "./schema.ts";

const snapshot = (value: unknown): any => JSON.parse(JSON.stringify(value));
const jsonl = (rows: unknown[]) => rows.map((row) => JSON.stringify(row)).join("\n") + (rows.length ? "\n" : "");
const allowedHeaders = new Set(["content-type", "x-session-id", "x-client-request-id", "anthropic-beta"]);

/** Calls the pinned provider directly. HTTP and SSE stay inside the SDK oracle. */
export async function recordWireScenario(input: Input<Scenario>, outputRoot: string) {
  const scenario = input.value as any;
  if (scenario.layer !== "wire" || scenario.provider.kind !== "wire" || !scenario.model.value) {
    throw new Error(`Unsupported wire scenario ${scenario.id}`);
  }
  if (scenario.tools?.length || scenario.variants?.length) throw new Error("Wire-only scenarios require explicit transcript tool declarations");
  const clock = deterministicEnvironment(scenario.clock.epochMs);
  try {
    const { stream } = await import("@earendil-works/pi-ai/api/openai-completions");
    const { normalizeContext } = await import("@earendil-works/pi-ai/utils/transcript");
    let model = scenario.model.value;
    const messages: any[] = snapshot(scenario.initialMessages ?? []);
    const results: any[] = [], events: any[] = [], requests: any[] = [], http: any[] = [], retained: any[] = [];
    const gates = new Map<string, { promise: Promise<void>; open: () => void }>();
    const gate = (name: string) => {
      if (!gates.has(name)) {
        let open!: () => void;
        const promise = new Promise<void>((resolve) => { open = resolve; });
        gates.set(name, { promise, open });
      }
      return gates.get(name)!;
    };
    let change = () => {};
    const emit = (type: string, data: unknown) => {
      events.push({ seq: events.length, type, entries: 0, data: snapshot(data) });
      change();
    };
    let nextResponse = 0, controller: AbortController | undefined, pending: Promise<void> | undefined, active = false;
    const scriptedFetch: typeof fetch = async (url, init) => {
      const fixture = scenario.provider.responses[nextResponse++];
      if (!fixture) throw new Error("Unexpected scripted fetch invocation");
      const headers = Object.fromEntries([...new Headers(init?.headers)].filter(([key]) => allowedHeaders.has(key)));
      const bodyRaw = String(init?.body ?? "");
      http.push({ call: http.length, method: init?.method ?? "GET", path: new URL(String(url)).pathname, headers, body: JSON.parse(bodyRaw), bodyRaw });
      if (fixture.networkError) throw new Error(fixture.networkError);
      if ("body" in fixture) return new Response(JSON.stringify(fixture.body), { status: fixture.status, headers: { "content-type": "application/json", ...fixture.headers } });
      let cursor = 0;
      const encoder = new TextEncoder();
      const body = new ReadableStream<Uint8Array>({
        async pull(target) {
          while (cursor < fixture.sse.length) {
            const chunk = fixture.sse[cursor++];
            if ("gate" in chunk) {
              if (init?.signal?.aborted) { target.close(); return; }
              await Promise.race([gate(chunk.gate).promise, new Promise<void>((resolve) => init?.signal?.addEventListener("abort", () => resolve(), { once: true }))]);
              if (init?.signal?.aborted) { target.close(); return; }
              continue;
            }
            if ("disconnect" in chunk) { target.error(new Error(chunk.disconnect)); return; }
            const text = "raw" in chunk ? chunk.raw : "comment" in chunk ? `: ${chunk.comment}\n\n`
              : `data: ${typeof chunk.data === "string" ? chunk.data : JSON.stringify(chunk.data)}\n\n`;
            target.enqueue(encoder.encode(text));
            return;
          }
          target.close();
        },
      });
      return new Response(body, { status: fixture.status, headers: { "content-type": "text/event-stream", ...fixture.headers } });
    };
    for (const step of scenario.steps) {
      emit("$step", step);
      if ("prompt" in step) {
        if (active) throw new Error("Prompt while wire stream is active");
        const prompt = step.prompt;
        messages.push(...(typeof prompt === "string" ? [{ role: "user", content: prompt, timestamp: Date.now() }] : Array.isArray(prompt) ? snapshot(prompt) : [snapshot(prompt)]));
        const context = normalizeContext({ messages: snapshot(messages), ...(scenario.systemPrompt === undefined ? {} : { systemPrompt: scenario.systemPrompt }) });
        controller = new AbortController();
        const options = { maxRetries: 0, ...scenario.options, apiKey: "fixture-key", signal: controller.signal, fetch: scriptedFetch };
        requests.push(snapshot({ call: requests.length, model, context, options: { ...options, apiKey: "<apiKey>", signal: undefined, fetch: undefined } }));
        const output = stream(model, context, options);
        active = true;
        pending = (async () => {
          for await (const event of output) { retained.push(event); emit(event.type, event); }
          const result = await output.result();
          messages.push(result);
          results.push(result);
          active = false;
          change();
        })();
      } else if ("setModel" in step) {
        if (active || !step.setModel.value) throw new Error("Wire model changes require an idle stream and explicit fixture value");
        model = step.setModel.value;
      } else if ("waitFor" in step) {
        const matches = () => events.filter((event) => event.type === step.waitFor.type).length >= (step.waitFor.count ?? 1);
        while (!matches()) {
          if (!active) throw new Error(`Wire stream ended before waitFor ${JSON.stringify(step.waitFor)}`);
          await new Promise<void>((resolve) => { change = resolve; });
        }
      } else if ("open" in step) gate(step.open).open();
      else if ("awaitIdle" in step) await pending;
      else if ("abort" in step) controller?.abort();
      else if ("advanceClock" in step) await clock.advance(step.advanceClock);
      else throw new Error(`Unsupported wire step ${Object.keys(step)[0]}`);
    }
    await pending;
    if (nextResponse !== scenario.provider.responses.length) throw new Error("Unconsumed wire responses");
    // Guard scenario coverage separately from recording: an unused fixture field
    // must not silently turn a tool/replay case into a plain-text case.
    if (scenario.id === "wire/streamed-tool-calls") {
      assert.equal(http[0].body.tools.length, 2);
      assert.equal(http[1].body.messages.filter((message: any) => message.role === "tool").length, 2);
    } else if (scenario.id === "wire/reasoning-fields") {
      assert(http[1].body.messages.some((message: any) => message.reasoning_details?.length));
    } else if (scenario.id === "wire/request-repair") {
      assert(http[0].body.messages.some((message: any) => message.content === "No result provided"));
      const body = http[0].bodyRaw;
      assert(body.includes("image omitted: model does not support images"));
      assert(body.includes("lone  unit"));
      assert(!body.includes("failed answer"));
    } else if (scenario.id === "wire/system-collapse-and-tools") {
      assert.equal(http[0].body.messages.filter((message: any) => message.role === "developer").length, 1);
      assert.equal(http[0].body.tools.length, 2);
      assert.equal(http[1].body.messages.filter((message: any) => message.role === "developer").length, 3);
      assert.equal(http[1].body.tools.length, 1);
      assert(http[1].bodyRaw.includes("third"));
    }
    for (let index = 0; index < results.length; index++) {
      const expected = scenario.provider.responses[index].transportError;
      if (expected !== undefined) assert.equal(results[index].errorMessage, expected);
    }
    const out = path.join(outputRoot, "scenarios", scenario.id);
    await mkdir(out, { recursive: true });
    await writeFile(path.join(out, "scenario.json"), input.source);
    await writeFile(path.join(out, "events.jsonl"), jsonl(events));
    await writeFile(path.join(out, "requests.jsonl"), jsonl(requests));
    await writeFile(path.join(out, "http.jsonl"), jsonl(http));
    await writeFile(path.join(out, "final.json"), JSON.stringify(snapshot({ state: { messages, results, retainedEvents: retained }, queues: {}, errors: [] }), null, 2) + "\n");
  } finally { clock.restore(); }
}
