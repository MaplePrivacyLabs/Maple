import { describe, expect, test } from "bun:test";
import type { BillingProduct, BillingStatus } from "./billingApi";
import {
  isCurrentProduct,
  isPaidPlan,
  isTeamPlan,
  monthlyCreditsText,
  planDisplayName,
  planGroup,
  planRank,
  productDisplayName,
  productGroup,
  productRank,
  productsByCard,
  type PlanCard
} from "./plan";

function status(fields: Partial<BillingStatus>): BillingStatus {
  return {
    is_subscribed: true,
    product_id: "prod_test",
    product_name: null,
    subscription_status: "active",
    current_period_end: null,
    can_chat: true,
    chats_remaining: null,
    payment_provider: "stripe",
    total_tokens: null,
    used_tokens: null,
    usage_reset_date: null,
    ...fields
  };
}

function product(fields: Partial<BillingProduct> & Pick<BillingProduct, "id" | "name">) {
  return {
    description: "",
    active: true,
    default_price: {
      id: `${fields.id}-monthly`,
      currency: "usd",
      unit_amount: 2000,
      recurring: { interval: "month", interval_count: 1 }
    },
    ...fields
  } satisfies BillingProduct;
}

const cards: PlanCard[] = [
  { name: "Free", planKey: "free", rank: 0 },
  { name: "Pro", planKey: "pro", rank: 1 },
  { name: "Max", planKey: "max", rank: 2 },
  { name: "Team", planKey: "team", rank: null }
];

describe("plans named only by an older billing server", () => {
  test.each([
    ["Free", "free", 0],
    [" free ", "free", 0],
    ["Pro", "individual", 1],
    ["Max", "individual", 2],
    ["Max Studio", "individual", 2],
    ["Team", "team", null],
    ["Pro Team", "team", null],
    ["Starter", null, null],
    [null, null, null]
  ] as const)("%s", (name, group, rank) => {
    const named = status({ product_name: name });
    expect(planGroup(named)).toBe(group);
    expect(planRank(named)).toBe(rank);
    expect(isPaidPlan(named)).toBe(group === "individual" || group === "team");
    expect(isTeamPlan(named)).toBe(group === "team");
  });

  test("no status is no plan", () => {
    for (const missing of [null, undefined]) {
      expect(planGroup(missing)).toBeNull();
      expect(planRank(missing)).toBeNull();
      expect(isPaidPlan(missing)).toBe(false);
      expect(planDisplayName(missing)).toBeNull();
    }
  });
});

describe("plans billing describes", () => {
  test("the description wins over the name", () => {
    // The name older builds read can say one thing while billing says another.
    const described = status({
      product_name: "Max Studio",
      plan_key: "studio",
      plan_group: "individual",
      plan_rank: 3,
      plan_display_name: "Studio"
    });
    expect(planGroup(described)).toBe("individual");
    expect(planRank(described)).toBe(3);
    expect(planDisplayName(described)).toBe("Studio");

    const free = status({ product_name: "Pro", plan_group: "free", plan_rank: 0 });
    expect(isPaidPlan(free)).toBe(false);
    expect(planRank(free)).toBe(0);
  });

  test("a group this build has never seen is still a paid plan", () => {
    const future = status({ product_name: "Starter", plan_group: "enterprise", plan_rank: null });
    expect(planGroup(future)).toBe("individual");
    expect(isPaidPlan(future)).toBe(true);
    expect(planRank(future)).toBeNull();
  });

  test("Team has no rank and a blank display name falls back", () => {
    const team = status({
      product_name: "Team",
      plan_group: "team",
      plan_rank: null,
      plan_display_name: "  "
    });
    expect(isTeamPlan(team)).toBe(true);
    expect(planRank(team)).toBeNull();
    expect(planDisplayName(team)).toBe("Team");
  });
});

describe("products", () => {
  test("describe themselves the same way", () => {
    const named = product({ id: "prod_max", name: "Max" });
    expect([productGroup(named), productRank(named), productDisplayName(named)]).toEqual([
      "individual",
      2,
      "Max"
    ]);
    const described = product({
      id: "prod_studio",
      name: "Max Studio",
      plan_group: "individual",
      plan_rank: 3,
      display_name: "Studio"
    });
    expect([
      productGroup(described),
      productRank(described),
      productDisplayName(described)
    ]).toEqual(["individual", 3, "Studio"]);
  });

  test("the current plan is matched by product, then by name", () => {
    const current = status({ product_id: "prod_max", product_name: "Max" });
    expect(isCurrentProduct(current, product({ id: "prod_max", name: "Renamed" }))).toBe(true);
    expect(isCurrentProduct(current, product({ id: "other", name: " max " }))).toBe(true);
    expect(isCurrentProduct(current, product({ id: "other", name: "Max Studio" }))).toBe(false);
    expect(isCurrentProduct(null, product({ id: "prod_max", name: "Max" }))).toBe(false);
  });

  test("an allowance reads as credits and a multiple of Pro when it is one", () => {
    const studio = product({ id: "prod_studio", name: "Max Studio", monthly_credits: 60_000 });
    expect(monthlyCreditsText(studio, 20_000)).toBe("60,000 monthly credits (3x Pro)");
    expect(monthlyCreditsText(studio, 25_000)).toBe("60,000 monthly credits");
    expect(monthlyCreditsText(studio)).toBe("60,000 monthly credits");
    expect(monthlyCreditsText(product({ id: "prod_max", name: "Max" }), 20_000)).toBeNull();
  });
});

describe("pricing cards", () => {
  const free = product({ id: "prod_free", name: "Free" });
  const pro = product({ id: "prod_pro", name: "Pro", plan_key: "pro", plan_rank: 1 });
  const max = product({ id: "prod_max", name: "Max", plan_key: "max", plan_rank: 2 });
  const team = product({ id: "prod_team", name: "Team", plan_key: "team", plan_group: "team" });
  const studio = product({
    id: "prod_studio",
    name: "Max Studio",
    plan_key: "studio",
    plan_group: "individual",
    plan_rank: 3
  });
  const ids = (offered: Map<string, BillingProduct[]>) =>
    Object.fromEntries([...offered].map(([card, list]) => [card, list.map(({ id }) => id)]));

  test("each card offers its own plan, by key or by name", () => {
    expect(ids(productsByCard(cards, [team, max, free, pro]))).toEqual({
      Free: ["prod_free"],
      Pro: ["prod_pro"],
      Max: ["prod_max"],
      Team: ["prod_team"]
    });
    // An older billing server sends names only.
    const named = [product({ id: "a", name: "pro" }), product({ id: "b", name: "Max" })];
    expect(ids(productsByCard(cards, named))).toEqual({
      Free: [],
      Pro: ["a"],
      Max: ["b"],
      Team: []
    });
  });

  test("a plan without a card becomes a larger size of the card below it", () => {
    expect(ids(productsByCard(cards, [studio, team, max, free, pro])).Max).toEqual([
      "prod_max",
      "prod_studio"
    ]);
    const between = product({
      id: "prod_between",
      name: "Pro Extra",
      plan_key: "pro_extra",
      plan_group: "individual",
      plan_rank: 1.5
    });
    const offered = ids(productsByCard(cards, [between, studio, max, pro]));
    expect(offered.Pro).toEqual(["prod_pro", "prod_between"]);
    expect(offered.Max).toEqual(["prod_max", "prod_studio"]);
  });

  test("plans that cannot be placed safely are left out", () => {
    const inactive = { ...studio, id: "prod_inactive", active: false };
    const unranked = { ...studio, id: "prod_unranked", plan_rank: null };
    const undescribed = product({ id: "prod_named", name: "Max Studio" });
    const teamSized = { ...studio, id: "prod_team_size", plan_group: "team" };
    const offered = productsByCard(cards, [inactive, unranked, undescribed, teamSized, max, pro]);
    expect(ids(offered)).toEqual({ Free: [], Pro: ["prod_pro"], Max: ["prod_max"], Team: [] });
    // Without Max on sale there is no card to offer a larger size of it.
    expect(ids(productsByCard(cards, [studio, pro])).Max).toEqual([]);
  });
});
