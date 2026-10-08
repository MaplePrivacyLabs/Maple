import type { Json } from "./schema.ts";

/** Observe the actual low-level Agent after explicitly clearing its model. */
export async function recordClearedModel(input: Record<string, Json>): Promise<unknown> {
  const { Agent } = await import("@earendil-works/pi-agent-core");
  const events: string[] = [];
  let providerInvocations = 0, apiKeyInvocations = 0;
  const agent = new Agent({
    streamFn(model) {
      providerInvocations++;
      throw new Error(model === undefined ? "provider received undefined" : "unexpected model");
    },
    ...(input.getApiKey ? { getApiKey() {
      apiKeyInvocations++;
      throw new Error("unexpected api key call");
    } } : {}),
  });
  agent.subscribe(event => { events.push(event.type); });
  const initialModel = agent.state.model.id;
  agent.state.model = undefined as never;
  let error: { name: string; message: string } | undefined;
  try { await agent.prompt(input.prompt as never); }
  catch (caught) {
    if (!(caught instanceof Error)) throw caught;
    error = { name: caught.name, message: caught.message };
  }
  await agent.waitForIdle();
  return {
    id: input.id, initialModel, events, providerInvocations, apiKeyInvocations, error,
    state: {
      modelMissing: agent.state.model === undefined,
      isStreaming: agent.state.isStreaming,
      errorMessage: agent.state.errorMessage,
      messages: agent.state.messages,
    },
  };
}
