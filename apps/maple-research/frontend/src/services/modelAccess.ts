import type { ModelAccess, ModelCatalogV2Response } from "@mapleai/sdk";
import type { OpenSecretModel, OpenSecretModelCatalog } from "@/state/LocalStateContextDef";

export type ModelPlanId = ModelAccess["eligible_plans"][number];
export type ModelUpgradeRequirement = { model: string; eligiblePlans: ModelPlanId[] };

export function planLabel(plans: readonly ModelPlanId[]): string {
  const labels: Record<ModelPlanId, string> = {
    free: "Free",
    pro: "Pro",
    max: "Max",
    teams: "Team"
  };
  // Pro already includes the higher subscription tiers.
  if (plans.includes("pro")) return "Pro or above";
  return plans.map((plan) => labels[plan]).join(" or ");
}

export function normalizeModelCatalogV2(catalog: ModelCatalogV2Response): OpenSecretModelCatalog {
  const adapt = <T extends { access: ModelAccess }>(model: T) => {
    const { access, ...metadata } = model;
    return {
      ...metadata,
      access: access.eligible_plans.includes("free") ? ("free" as const) : ("pro" as const),
      accessPolicy: access
    };
  };
  return { ...catalog, data: catalog.data.map(adapt), aliases: catalog.aliases.map(adapt) };
}

export function canUseModel(
  model: Pick<OpenSecretModel, "access" | "accessPolicy"> | undefined,
  productName?: string | null
): boolean {
  if (model?.accessPolicy) return model.accessPolicy.allowed;
  if (!model?.access || model.access === "free") return true;
  const plan = productName?.toLowerCase() || "";
  return plan.includes("pro") || plan.includes("max") || plan.includes("team");
}

export function isCatalogEndpointAbsent(error: unknown): boolean {
  if (typeof error !== "object" || error === null) return false;
  const status = (error as { status?: unknown }).status;
  return status === 404 || status === 405;
}
