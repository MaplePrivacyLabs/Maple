import type { OpenSecretModel, OpenSecretModelCatalog } from "@/state/LocalStateContextDef";

export type ProxyModelClient = {
  fetchModelCatalog: () => Promise<OpenSecretModelCatalog>;
  fetchModels: () => Promise<OpenSecretModel[]>;
};

export function isProxyChatModel(model: OpenSecretModel): boolean {
  if (!model.id.trim() || model.enabled === false || model.deprecated === true) return false;
  if (model.capabilities?.chat === false) return false;

  const tasks = model.tasks ?? [];
  if (tasks.length > 0) return tasks.includes("generate");

  const normalizedId = model.id.toLowerCase();
  return !["whisper", "transcri", "embed", "speech", "tts"].some((part) =>
    normalizedId.includes(part)
  );
}

export function normalizeProxyChatModels(models: OpenSecretModel[]): OpenSecretModel[] {
  const deduplicated = new Map<string, { model: OpenSecretModel; originalIndex: number }>();

  models.forEach((model, originalIndex) => {
    if (!isProxyChatModel(model) || deduplicated.has(model.id)) return;
    deduplicated.set(model.id, { model, originalIndex });
  });

  return [...deduplicated.values()]
    .sort((left, right) => {
      const leftOrder = left.model.sort_order ?? Number.MAX_SAFE_INTEGER;
      const rightOrder = right.model.sort_order ?? Number.MAX_SAFE_INTEGER;
      return leftOrder - rightOrder || left.originalIndex - right.originalIndex;
    })
    .map(({ model }) => model);
}

export function isCodingAgentModel(model: OpenSecretModel): boolean {
  return isProxyChatModel(model) && model.capabilities?.tool_use === true;
}

export async function fetchProxyChatModels(client: ProxyModelClient): Promise<OpenSecretModel[]> {
  try {
    const catalog = await client.fetchModelCatalog();
    return normalizeProxyChatModels(catalog.data);
  } catch (catalogError) {
    try {
      return normalizeProxyChatModels(await client.fetchModels());
    } catch {
      throw catalogError;
    }
  }
}

export function getProxyBaseUrl(host: string, port: number): string {
  const trimmedHost = host.trim() || "127.0.0.1";
  const clientHost = trimmedHost === "0.0.0.0" || trimmedHost === "::" ? "127.0.0.1" : trimmedHost;
  const formattedHost = clientHost.includes(":") ? `[${clientHost}]` : clientHost;
  return `http://${formattedHost}:${port}/v1`;
}

export function getModelDisplayName(model: OpenSecretModel): string {
  return model.display_name || model.short_name || model.id;
}

const OPENCODE_PROVIDER_ID = "maple-local";
// OpenCode's own output cap for a model whose limit it does not know.
const OPENCODE_OUTPUT_LIMIT = 32_000;

export function buildOpenCodeProviderFields(baseUrl: string, model?: OpenSecretModel) {
  return {
    providerId: OPENCODE_PROVIDER_ID,
    displayName: "Maple (local proxy)",
    baseUrl,
    apiKey: "Paste the real Maple key you created",
    modelId: model?.id ?? "SELECT_A_MODEL",
    modelName: model ? getModelDisplayName(model) : "Select a model",
    headers: "Leave blank"
  };
}

/**
 * The `opencode.json` block that gives OpenCode this model's context window.
 * OpenCode cannot read the window from a custom provider, and without it a long
 * session can grow until the model rejects it. Null when the catalog has no
 * window for the model.
 */
export function buildOpenCodeLimitConfig(model?: OpenSecretModel): string | null {
  const context = model?.context_window ?? model?.max_context_tokens;
  if (!model || !context) return null;
  const output = Math.min(OPENCODE_OUTPUT_LIMIT, Math.floor(context / 4));
  return JSON.stringify(
    {
      provider: {
        [OPENCODE_PROVIDER_ID]: { models: { [model.id]: { limit: { context, output } } } }
      }
    },
    null,
    2
  );
}

export function formatModelContext(model: OpenSecretModel): string | null {
  const context = model.context_window ?? model.max_context_tokens;
  if (!context) return null;
  if (context >= 1_000_000) return `${Math.round(context / 100_000) / 10}M context`;
  if (context >= 1_000) return `${Math.round(context / 1_000)}K context`;
  return `${context.toLocaleString()} context`;
}

export function buildCurlProxyExample(baseUrl: string, modelId: string): string {
  return `curl --fail-with-body -N ${baseUrl}/chat/completions \\
  -H "Authorization: Bearer $MAPLE_API_KEY" \\
  -H "Content-Type: application/json" \\
  -d '${JSON.stringify({
    model: modelId,
    messages: [{ role: "user", content: "Hello!" }],
    stream: true
  })}'`;
}

export function buildPythonProxyExample(baseUrl: string, modelId: string): string {
  return `import os
from openai import OpenAI

client = OpenAI(
    base_url="${baseUrl}",
    api_key=os.environ["MAPLE_API_KEY"],
)

stream = client.chat.completions.create(
    model="${modelId}",
    messages=[{"role": "user", "content": "Hello!"}],
    stream=True,
)

for chunk in stream:
    print(chunk.choices[0].delta.content or "", end="")`;
}
