import { describe, expect, test } from "bun:test";
import type {
  ModelReasoning,
  OpenSecretModel,
  OpenSecretModelAlias
} from "@/state/LocalStateContextDef";
import {
  getStoredThinkingLevel,
  resolveModelReasoning,
  resolveThinkingLevel,
  storeThinkingLevel,
  thinkingLevelLabel,
  thinkingLevelOptions
} from "./chatThinkingLevel";

const GLM: ModelReasoning = {
  mandatory: true,
  default_enabled: true,
  supported_efforts: ["max", "high", "low"],
  default_effort: "max"
};
const KIMI: ModelReasoning = {
  mandatory: false,
  default_enabled: true,
  supported_efforts: ["max", "high", "low"],
  default_effort: "max"
};
const GPT_OSS: ModelReasoning = {
  mandatory: true,
  default_enabled: true,
  supported_efforts: ["high", "medium", "low"],
  default_effort: "medium"
};
const GEMMA: ModelReasoning = { mandatory: false, default_enabled: false };

function values(reasoning: ModelReasoning | null | undefined): string[] {
  return thinkingLevelOptions(reasoning).map((option) => option.value);
}

describe("thinkingLevelOptions", () => {
  test("hides the control for models that never reason", () => {
    expect(values(undefined)).toEqual([]);
    expect(values(null)).toEqual([]);
  });

  test("lists default, then off where allowed, then efforts from low to high", () => {
    expect(values(GLM)).toEqual(["default", "low", "high", "max"]);
    expect(values(KIMI)).toEqual(["default", "none", "low", "high", "max"]);
    expect(values(GPT_OSS)).toEqual(["default", "low", "medium", "high"]);
    expect(values(GEMMA)).toEqual(["default", "none"]);
  });

  test("describes the default with the model's own default effort", () => {
    expect(thinkingLevelOptions(GLM)[0].description).toBe("Model default (max)");
    expect(thinkingLevelOptions(GEMMA)[0].description).toBeUndefined();
    expect(thinkingLevelLabel("xhigh")).toBe("Extra high");
    expect(thinkingLevelLabel("none")).toBe("Off");
    expect(thinkingLevelLabel("default")).toBe("Default");
  });
});

describe("resolveThinkingLevel", () => {
  test("sends only levels the selected model publishes", () => {
    expect(resolveThinkingLevel("low", GLM).effort).toBe("low");
    expect(resolveThinkingLevel("default", GLM).effort).toBeUndefined();
    // A stored medium came from another model; GLM falls back to its default.
    const medium = resolveThinkingLevel("medium", GLM);
    expect(medium.effective).toBe("default");
    expect(medium.effort).toBeUndefined();
    // Off is only offered where reasoning is not mandatory.
    expect(resolveThinkingLevel("none", GLM).effort).toBeUndefined();
    expect(resolveThinkingLevel("none", KIMI).effort).toBe("none");
    expect(resolveThinkingLevel("none", GEMMA).effort).toBe("none");
    expect(resolveThinkingLevel("high", GEMMA).effective).toBe("default");
  });

  test("never sends an effort for a model without reasoning", () => {
    const resolved = resolveThinkingLevel("max", null);
    expect(resolved.options).toEqual([]);
    expect(resolved.effective).toBe("default");
    expect(resolved.effort).toBeUndefined();
  });
});

describe("resolveModelReasoning", () => {
  const models: OpenSecretModel[] = [
    { id: "glm-5-3", object: "model", created: 0, owned_by: "opensecret", reasoning: GLM },
    { id: "llama3-3-70b", object: "model", created: 0, owned_by: "opensecret" },
    { id: "old-server", object: "model", created: 0, owned_by: "opensecret", reasoning: null }
  ];
  const aliases: OpenSecretModelAlias[] = [
    {
      id: "auto:quick",
      label: "Quick",
      short_name: "Quick",
      description: "",
      target_model: "glm-5-3-flash",
      reasoning: GLM
    }
  ];

  test("prefers the alias entry, then the model entry", () => {
    expect(resolveModelReasoning("auto:quick", models, aliases)).toEqual(GLM);
    expect(resolveModelReasoning("glm-5-3", models, aliases)).toEqual(GLM);
  });

  test("is undefined for unknown or metadata-less models and null when published as null", () => {
    expect(resolveModelReasoning("llama3-3-70b", models, aliases)).toBeUndefined();
    expect(resolveModelReasoning("missing", models, aliases)).toBeUndefined();
    expect(resolveModelReasoning("old-server", models, aliases)).toBeNull();
  });
});

describe("stored thinking level", () => {
  function memoryStorage(initial: Record<string, string> = {}) {
    const store = new Map(Object.entries(initial));
    return {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => {
        store.set(key, value);
      }
    };
  }

  test("round-trips valid choices and ignores anything else", () => {
    const storage = memoryStorage();
    expect(getStoredThinkingLevel(storage)).toBe("default");
    storeThinkingLevel("high", storage);
    expect(getStoredThinkingLevel(storage)).toBe("high");
    expect(getStoredThinkingLevel(memoryStorage({ chatThinkingLevel: "ultra" }))).toBe("default");
    expect(getStoredThinkingLevel(null)).toBe("default");
  });
});
