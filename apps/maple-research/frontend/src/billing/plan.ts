import type { BillingProduct, BillingStatus } from "./billingApi";

/**
 * Plan identity. Billing describes each plan with `plan_group`, `plan_rank` and
 * a display name. Older billing servers send only names, and so does billing
 * for a product it does not recognize, so every reader here falls back to the
 * name the way this app always has.
 */
export type PlanGroup = "free" | "individual" | "team";

type StatusPlan = Pick<
  BillingStatus,
  "product_id" | "product_name" | "plan_group" | "plan_rank" | "plan_display_name"
>;

type ProductPlan = Pick<BillingProduct, "id" | "name"> &
  Partial<
    Pick<
      BillingProduct,
      "active" | "plan_key" | "plan_group" | "plan_rank" | "display_name" | "monthly_credits"
    >
  >;

function describedGroup(group: string | undefined): PlanGroup | null {
  if (!group) return null;
  if (group === "free" || group === "team") return group;
  // Anything billing describes that is not Free is a paid plan, including a
  // group this build has never seen.
  return "individual";
}

function groupFromName(name: string | null | undefined): PlanGroup | null {
  const lowered = name?.trim().toLowerCase() ?? "";
  if (lowered === "free") return "free";
  if (lowered.includes("team")) return "team";
  if (lowered.includes("pro") || lowered.includes("max")) return "individual";
  return null;
}

function rankFromName(name: string | null | undefined): number | null {
  const lowered = name?.trim().toLowerCase() ?? "";
  if (lowered === "free") return 0;
  if (lowered.includes("team")) return null;
  if (lowered.includes("max")) return 2;
  if (lowered.includes("pro")) return 1;
  return null;
}

/** `null` when there is no status or the plan cannot be told. */
export function planGroup(status: StatusPlan | null | undefined): PlanGroup | null {
  if (!status) return null;
  return describedGroup(status.plan_group) ?? groupFromName(status.product_name);
}

export function isPaidPlan(status: StatusPlan | null | undefined): boolean {
  const group = planGroup(status);
  return group === "individual" || group === "team";
}

export function isTeamPlan(status: StatusPlan | null | undefined): boolean {
  return planGroup(status) === "team";
}

/** Order among Free and the individual plans; `null` for Team and unknown plans. */
export function planRank(status: StatusPlan | null | undefined): number | null {
  if (!status) return null;
  if (typeof status.plan_rank === "number") return status.plan_rank;
  if (status.plan_group) return null;
  return rankFromName(status.product_name);
}

/** The plan name to show. `product_name` is kept for older builds and can differ. */
export function planDisplayName(status: StatusPlan | null | undefined): string | null {
  return status?.plan_display_name?.trim() || status?.product_name || null;
}

export function productGroup(product: ProductPlan): PlanGroup | null {
  return describedGroup(product.plan_group) ?? groupFromName(product.name);
}

export function productRank(product: ProductPlan): number | null {
  if (typeof product.plan_rank === "number") return product.plan_rank;
  if (product.plan_group) return null;
  return rankFromName(product.name);
}

export function productDisplayName(product: ProductPlan): string {
  return product.display_name?.trim() || product.name;
}

/** Whether the account's plan is this product. */
export function isCurrentProduct(
  status: StatusPlan | null | undefined,
  product: ProductPlan
): boolean {
  if (!status) return false;
  if (status.product_id === product.id) return true;
  const current = status.product_name?.trim().toLowerCase();
  return !!current && current === product.name.trim().toLowerCase();
}

export type PlanCard = {
  name: string;
  planKey: string;
  /** Order among the individual cards; `null` for Team. */
  rank: number | null;
};

/**
 * The products each pricing card offers. A card's own plan comes first. An
 * individual plan billing lists that has no card of its own is offered by the
 * highest-ranked paid card below it, as a larger size of that plan, so a plan
 * added later shows up without a release.
 */
export function productsByCard<P extends ProductPlan>(
  cards: readonly PlanCard[],
  products: readonly P[]
): Map<string, P[]> {
  const offered = new Map<string, P[]>(cards.map((card) => [card.name, []]));
  const extra: P[] = [];
  for (const product of products) {
    if (product.active === false) continue;
    const own = cards.find((card) =>
      product.plan_key
        ? product.plan_key === card.planKey
        : product.name.trim().toLowerCase() === card.name.toLowerCase()
    );
    if (own) offered.get(own.name)!.push(product);
    else extra.push(product);
  }
  for (const product of extra) {
    const rank = product.plan_rank;
    if (product.plan_group !== "individual" || typeof rank !== "number") continue;
    const host = cards
      .filter((card) => card.rank !== null && card.rank >= 1 && card.rank < rank)
      .sort((a, b) => b.rank! - a.rank!)[0];
    // A larger size needs the plan it extends to be on sale.
    if (host && offered.get(host.name)!.length > 0) offered.get(host.name)!.push(product);
  }
  for (const [name, list] of offered) {
    const card = cards.find((candidate) => candidate.name === name)!;
    const order = (product: P) =>
      product.plan_key === card.planKey || typeof product.plan_rank !== "number"
        ? -1
        : product.plan_rank;
    list.sort((a, b) => order(a) - order(b));
  }
  return offered;
}

/** "60,000 monthly credits (3x Pro)" for a product billing gave an allowance. */
export function monthlyCreditsText(product: ProductPlan, proCredits?: number): string | null {
  const credits = product.monthly_credits;
  if (typeof credits !== "number" || credits <= 0) return null;
  const amount = credits.toLocaleString("en-US");
  const multiple = proCredits && credits % proCredits === 0 ? credits / proCredits : 0;
  return multiple > 1
    ? `${amount} monthly credits (${multiple}x Pro)`
    : `${amount} monthly credits`;
}
