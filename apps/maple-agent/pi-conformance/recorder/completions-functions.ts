import "./network-guard.ts";
import type { Json } from "./schema.ts";

/** The caller installs the deterministic clock before importing/calling Pi. */
export async function prepareCompletionsFunction(id: string, value: Record<string, Json>): Promise<() => Promise<unknown>> {
  const { normalizeContext } = await import("@earendil-works/pi-ai/utils/transcript");
  const { transformMessages } = await import("@earendil-works/pi-ai/api/transform-messages");
  const { stream, convertMessages } = await import("@earendil-works/pi-ai/api/openai-completions");
  return async () => {
    if (id === "api.transformMessages") {
      return transformMessages(value.messages as any, value.model as any,
        value.normalizeToolCallId === "prefix" ? (id) => `normalized:${id}` : undefined);
    }
    const context = normalizeContext(value.context as any);
    if (id === "api.convertMessages") return convertMessages(value.model as any, context, value.compat as any);
    if (id !== "api.buildParams") throw new Error(`Unsupported completions function: ${id}`);
    let captured: unknown;
    let fetchCalls = 0;
    const result = await stream(value.model as any, context, {
      ...value.options as any,
      apiKey: "fixture-key",
      onPayload(payload) {
        captured = structuredClone(payload);
        throw new Error("Request-builder capture complete");
      },
      fetch: async () => {
        fetchCalls++;
        throw new Error("Request-builder capture unexpectedly reached fetch");
      },
    }).result();
    if (fetchCalls !== 0) throw new Error("Request-builder capture reached fetch");
    if (captured === undefined) throw new Error(result.errorMessage);
    return captured;
  };
}
