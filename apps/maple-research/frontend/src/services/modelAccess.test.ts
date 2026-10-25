import { describe, expect, test } from "bun:test";
import {
  canUseModel,
  normalizeModelCatalogV2,
  planLabel,
  isCatalogEndpointAbsent
} from "./modelAccess";
import type { ModelCatalogV2Response } from "@mapleai/sdk";

describe("inclusive model access", () => {
  test("a server flag grant overrides the account's Free plan", () => {
    expect(
      canUseModel(
        {
          access: "pro",
          accessPolicy: {
            allowed: true,
            eligible_plans: ["max", "teams"],
            reason: null
          }
        },
        "Free"
      )
    ).toBe(true);
  });
  test("a Pro account cannot infer permission from paid-model metadata", () => {
    expect(
      canUseModel(
        {
          access: "pro",
          accessPolicy: {
            allowed: false,
            eligible_plans: ["max", "teams"],
            reason: "plan_upgrade_required"
          }
        },
        "Pro"
      )
    ).toBe(false);
  });
  test("v1 metadata keeps its existing paid-plan interpretation", () => {
    expect(canUseModel({ access: "pro" }, "Pro")).toBe(true);
    expect(canUseModel({ access: "pro" }, "Free")).toBe(false);
  });
  test("v2 metadata remains attached when adapting to existing model state", () => {
    const catalog = {
      object: "list",
      data: [
        {
          id: "uncensored",
          access: {
            allowed: false,
            eligible_plans: ["max", "teams"],
            reason: "plan_upgrade_required"
          }
        }
      ],
      aliases: [],
      defaults: { quick: "auto:quick", powerful: "auto:powerful" }
    } as unknown as ModelCatalogV2Response;
    const model = normalizeModelCatalogV2(catalog).data[0];
    expect(model.accessPolicy).toEqual(catalog.data[0].access);
    expect(canUseModel(model, "Pro")).toBe(false);
  });
  test("labels and endpoint fallback do not turn denial or outages into upgrades", () => {
    expect(planLabel(["max", "teams"])).toBe("Max or Team");
    expect(planLabel(["pro", "max", "teams"])).toBe("Pro or above");
    for (const status of [401, 403, 429, 500, 503])
      expect(isCatalogEndpointAbsent({ status })).toBe(false);
    expect(isCatalogEndpointAbsent({ status: 404 })).toBe(true);
    expect(isCatalogEndpointAbsent({ status: 405 })).toBe(true);
  });
});
