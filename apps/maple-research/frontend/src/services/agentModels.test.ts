import { describe, expect, test } from "bun:test";
import { POWERFUL_MODEL_ALIAS, QUICK_MODEL_ALIAS } from "@/utils/utils";
import {
  DEFAULT_AGENT_MODEL,
  PRIMARY_AGENT_MODEL_IDS,
  PREVIOUS_RECOMMENDED_AGENT_MODEL,
  fallbackAgentModel,
  migrateAgentModelPreference,
  reconcileAgentModel,
  reconcileAgentModelForCatalog,
  resolveAgentModelForSession,
  resolveAgentModelContextLimit,
  resolveAgentModelVisionCapability
} from "./agentModels";

const models = [
  { id: PREVIOUS_RECOMMENDED_AGENT_MODEL },
  { id: "glm-5-3" },
  { id: "deepseek-v4-1-flash" },
  { id: "kimi-k2-6" }
];

describe("Agent Mode model defaults", () => {
  test("uses the same Quick/Powerful primary choices as chat", () => {
    expect(DEFAULT_AGENT_MODEL).toBe(QUICK_MODEL_ALIAS);
    expect(PRIMARY_AGENT_MODEL_IDS).toEqual([QUICK_MODEL_ALIAS, POWERFUL_MODEL_ALIAS]);
    expect(PRIMARY_AGENT_MODEL_IDS).not.toContain(PREVIOUS_RECOMMENDED_AGENT_MODEL);
    expect(PRIMARY_AGENT_MODEL_IDS).not.toContain("kimi-k2-6");
  });

  test("falls back to Quick even when a previous recommended concrete model is available", () => {
    expect(fallbackAgentModel()).toBe(QUICK_MODEL_ALIAS);
  });

  test("clears the previous Agent-only recommended model from sticky preferences", () => {
    expect(migrateAgentModelPreference(PREVIOUS_RECOMMENDED_AGENT_MODEL)).toBeNull();
    expect(migrateAgentModelPreference("  glm-5-2  ")).toBeNull();
    expect(migrateAgentModelPreference("deepseek-v4-flash")).toBeNull();
    expect(migrateAgentModelPreference(null)).toBeNull();
    expect(migrateAgentModelPreference("kimi-k2-6")).toBe("kimi-k2-6");
    expect(migrateAgentModelPreference(POWERFUL_MODEL_ALIAS)).toBe(POWERFUL_MODEL_ALIAS);
  });

  test("keeps selectable concrete models and existing aliases", () => {
    expect(reconcileAgentModel("kimi-k2-6", models)).toBe("kimi-k2-6");
    expect(reconcileAgentModel(QUICK_MODEL_ALIAS, models)).toBe(QUICK_MODEL_ALIAS);
    expect(reconcileAgentModel(POWERFUL_MODEL_ALIAS, models)).toBe(POWERFUL_MODEL_ALIAS);
  });

  test("replaces a missing concrete model with the shared Quick default", () => {
    expect(reconcileAgentModel("retired-model", models)).toBe(QUICK_MODEL_ALIAS);
    expect(reconcileAgentModel(PREVIOUS_RECOMMENDED_AGENT_MODEL, models)).toBe(
      PREVIOUS_RECOMMENDED_AGENT_MODEL
    );
    expect(reconcileAgentModel(PREVIOUS_RECOMMENDED_AGENT_MODEL, [{ id: "kimi-k2-6" }])).toBe(
      QUICK_MODEL_ALIAS
    );
  });

  test("catalog refresh preserves a started task's locked model", () => {
    expect(reconcileAgentModelForCatalog("retired-model", models, true)).toBe("retired-model");
    expect(reconcileAgentModelForCatalog("retired-model", models, false)).toBe(QUICK_MODEL_ALIAS);
  });

  test("uses remembered models for new tasks without changing started tasks", () => {
    expect(resolveAgentModelForSession("remembered", "session", false)).toBe("remembered");
    expect(resolveAgentModelForSession("remembered", "session", true)).toBe("session");
  });
});

describe("resolveAgentModelVisionCapability", () => {
  const catalog = [
    { id: "glm", capabilities: { vision: false } },
    { id: "gemma", capabilities: { vision: true } }
  ];

  test("uses the concrete model capability", () => {
    expect(resolveAgentModelVisionCapability("gemma", catalog, [])).toBe(true);
    expect(resolveAgentModelVisionCapability("glm", catalog, [])).toBe(false);
  });

  test("resolves aliases through their current catalog target", () => {
    const aliases = [
      {
        id: QUICK_MODEL_ALIAS,
        target_model: "gemma",
        capabilities: { vision: false }
      }
    ];

    expect(resolveAgentModelVisionCapability(QUICK_MODEL_ALIAS, catalog, aliases)).toBe(true);
  });

  test("uses alias metadata when its target is unavailable", () => {
    const aliases = [
      {
        id: POWERFUL_MODEL_ALIAS,
        target_model: "missing",
        capabilities: { vision: true }
      }
    ];

    expect(resolveAgentModelVisionCapability(POWERFUL_MODEL_ALIAS, catalog, aliases)).toBe(true);
  });

  test("fails closed when the model capability is unknown", () => {
    expect(resolveAgentModelVisionCapability("unknown", catalog, [])).toBe(false);
    expect(resolveAgentModelVisionCapability(QUICK_MODEL_ALIAS, catalog, [])).toBe(false);
  });
});

describe("resolveAgentModelContextLimit", () => {
  const catalog = {
    data: [
      { id: "glm-5-2", context_window: 384_000, max_context_tokens: 384_000 },
      { id: "kimi-k2-6", context_window: 256_000, max_context_tokens: 256_000 },
      { id: "gemma4-31b", context_window: 256_000, max_context_tokens: 256_000 }
    ],
    aliases: [
      { id: QUICK_MODEL_ALIAS, target_model: "glm-5-2" },
      { id: POWERFUL_MODEL_ALIAS, target_model: "kimi-k2-6" }
    ]
  };

  test("uses exact concrete-model context windows", () => {
    expect(resolveAgentModelContextLimit("glm-5-2", catalog)).toBe(384_000);
    expect(resolveAgentModelContextLimit("kimi-k2-6", catalog)).toBe(256_000);
    expect(resolveAgentModelContextLimit("gemma4-31b", catalog)).toBe(256_000);
  });

  test("resolves aliases through their current concrete target", () => {
    expect(resolveAgentModelContextLimit(POWERFUL_MODEL_ALIAS, catalog)).toBe(256_000);
    expect(
      resolveAgentModelContextLimit(POWERFUL_MODEL_ALIAS, {
        ...catalog,
        aliases: [{ id: POWERFUL_MODEL_ALIAS, target_model: "glm-5-2" }]
      })
    ).toBe(384_000);
  });

  test("accepts either compatible field when the other is absent", () => {
    expect(
      resolveAgentModelContextLimit("context-window-only", {
        data: [{ id: "context-window-only", context_window: 384_000 }],
        aliases: []
      })
    ).toBe(384_000);
    expect(
      resolveAgentModelContextLimit("max-context-only", {
        data: [{ id: "max-context-only", max_context_tokens: 256_000 }],
        aliases: []
      })
    ).toBe(256_000);
  });

  test("fails closed for unavailable or inconsistent metadata", () => {
    expect(resolveAgentModelContextLimit("glm-5-2", null)).toBeUndefined();
    expect(resolveAgentModelContextLimit("missing", catalog)).toBeUndefined();
    expect(
      resolveAgentModelContextLimit(POWERFUL_MODEL_ALIAS, {
        data: catalog.data,
        aliases: [{ id: POWERFUL_MODEL_ALIAS, target_model: "missing" }]
      })
    ).toBeUndefined();
    expect(
      resolveAgentModelContextLimit("mismatch", {
        data: [{ id: "mismatch", context_window: 384_000, max_context_tokens: 256_000 }],
        aliases: []
      })
    ).toBeUndefined();
  });

  test.each([
    0,
    -1,
    1.5,
    Number.NaN,
    Number.POSITIVE_INFINITY,
    Number.MAX_SAFE_INTEGER + 1,
    "384000",
    null
  ])("rejects invalid context metadata %p", (contextWindow) => {
    expect(
      resolveAgentModelContextLimit("invalid", {
        data: [{ id: "invalid", context_window: contextWindow }],
        aliases: []
      })
    ).toBeUndefined();
  });
});
