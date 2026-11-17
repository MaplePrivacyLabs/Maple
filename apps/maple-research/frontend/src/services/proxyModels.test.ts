import { describe, expect, test } from "bun:test";
import type { OpenSecretModel } from "@/state/LocalStateContextDef";
import {
  buildOpenCodeLimitConfig,
  buildOpenCodeProviderFields,
  buildCurlProxyExample,
  buildPythonProxyExample,
  fetchProxyChatModels,
  formatModelContext,
  getProxyBaseUrl,
  isCodingAgentModel,
  normalizeProxyChatModels
} from "./proxyModels";

function model(id: string, overrides: Partial<OpenSecretModel> = {}): OpenSecretModel {
  return { id, object: "model", created: 0, owned_by: "opensecret", ...overrides };
}

describe("normalizeProxyChatModels", () => {
  test("keeps current generation models and filters unsupported utility entries", () => {
    const models = normalizeProxyChatModels([
      model("glm-current", { tasks: ["generate"], capabilities: { chat: true } }),
      model("embedding", { tasks: ["embed"] }),
      model("speech", { tasks: ["speech"] }),
      model("disabled", { tasks: ["generate"], enabled: false }),
      model("deprecated", { tasks: ["generate"], deprecated: true })
    ]);

    expect(models.map((candidate) => candidate.id)).toEqual(["glm-current"]);
  });

  test("deduplicates IDs and respects catalog ordering metadata", () => {
    const models = normalizeProxyChatModels([
      model("later", { tasks: ["generate"], sort_order: 20 }),
      model("first", { tasks: ["generate"], sort_order: 10 }),
      model("first", { tasks: ["generate"], sort_order: 1 })
    ]);

    expect(models.map((candidate) => candidate.id)).toEqual(["first", "later"]);
  });
});

describe("fetchProxyChatModels", () => {
  test("uses the richer catalog when it is available", async () => {
    const models = await fetchProxyChatModels({
      fetchModelCatalog: async () => ({
        object: "list",
        aliases: [],
        data: [model("catalog-model", { tasks: ["generate"] })]
      }),
      fetchModels: async () => [model("fallback-model", { tasks: ["generate"] })]
    });

    expect(models.map((candidate) => candidate.id)).toEqual(["catalog-model"]);
  });

  test("falls back to the basic model list when catalog loading fails", async () => {
    const models = await fetchProxyChatModels({
      fetchModelCatalog: async () => {
        throw new Error("catalog unavailable");
      },
      fetchModels: async () => [model("fallback-model", { tasks: ["generate"] })]
    });

    expect(models.map((candidate) => candidate.id)).toEqual(["fallback-model"]);
  });
});

describe("proxy guide values", () => {
  test("builds a reachable base URL for loopback and wildcard binds", () => {
    expect(getProxyBaseUrl("127.0.0.1", 8080)).toBe("http://127.0.0.1:8080/v1");
    expect(getProxyBaseUrl("0.0.0.0", 9000)).toBe("http://127.0.0.1:9000/v1");
  });

  test("uses a real-key environment variable and the selected current model", () => {
    const curl = buildCurlProxyExample("http://127.0.0.1:8080/v1", "current-model");
    const python = buildPythonProxyExample("http://127.0.0.1:8080/v1", "current-model");

    expect(curl).toContain("Bearer $MAPLE_API_KEY");
    expect(curl).toContain('"model":"current-model"');
    expect(python).toContain('os.environ["MAPLE_API_KEY"]');
    expect(python).toContain('model="current-model"');
    const retiredPlaceholderExample = ["api_key", '"anything"'].join("=");
    expect(`${curl}\n${python}`).not.toContain(retiredPlaceholderExample);
  });

  test("builds the verified OpenCode provider fields without placeholder credentials", () => {
    const fields = buildOpenCodeProviderFields(
      "http://127.0.0.1:8080/v1",
      model("current-model", { display_name: "Current Model" })
    );

    expect(fields).toEqual({
      providerId: "maple-local",
      displayName: "Maple (local proxy)",
      baseUrl: "http://127.0.0.1:8080/v1",
      apiKey: "Paste the real Maple key you created",
      modelId: "current-model",
      modelName: "Current Model",
      headers: "Leave blank"
    });
    expect(Object.values(fields).join("\n")).not.toContain("anything");
  });

  test("builds the OpenCode context limit from the catalog window", () => {
    const config = buildOpenCodeLimitConfig(model("kimi-k3-1-pro", { context_window: 262_144 }));
    expect(JSON.parse(config!)).toEqual({
      provider: {
        "maple-local": {
          models: { "kimi-k3-1-pro": { limit: { context: 262_144, output: 32_000 } } }
        }
      }
    });
    // A small window still leaves most of itself for the conversation.
    const small = buildOpenCodeLimitConfig(model("small", { max_context_tokens: 32_768 }));
    expect(JSON.parse(small!).provider["maple-local"].models.small.limit).toEqual({
      context: 32_768,
      output: 8_192
    });
    expect(buildOpenCodeLimitConfig(model("unknown"))).toBeNull();
    expect(buildOpenCodeLimitConfig()).toBeNull();
  });

  test("only recommends models that explicitly support coding tools", () => {
    expect(
      isCodingAgentModel(
        model("tools", { tasks: ["generate"], capabilities: { chat: true, tool_use: true } })
      )
    ).toBe(true);
    expect(isCodingAgentModel(model("unknown", { tasks: ["generate"] }))).toBe(false);
    expect(
      isCodingAgentModel(
        model("no-tools", { tasks: ["generate"], capabilities: { chat: true, tool_use: false } })
      )
    ).toBe(false);
  });

  test("formats context windows without inventing metadata", () => {
    expect(formatModelContext(model("large", { context_window: 384_000 }))).toBe("384K context");
    expect(formatModelContext(model("unknown"))).toBeNull();
  });
});
