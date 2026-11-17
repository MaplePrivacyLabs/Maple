import { describe, expect, test } from "bun:test";
import type { BillingProduct, BillingStatus } from "./billingApi";
import {
  appleProductIds,
  applePurchaseConflict,
  appleSizes,
  currentApplePlan,
  currentAppleSize,
  hasAppleSubscriptionToManage,
  hasConfirmedBillingStatus,
  monthlyAppleProducts,
  monthlyAppleSizeProducts
} from "./applePricingPolicy";
import type { StoreKitProduct } from "@/services/storeKitService";

const free: BillingStatus = {
  is_subscribed: true,
  stripe_customer_id: null,
  product_id: "free",
  product_name: "Free",
  subscription_status: "active",
  current_period_end: null,
  can_chat: true,
  chats_remaining: 25,
  payment_provider: null,
  total_tokens: null,
  used_tokens: null,
  usage_reset_date: null,
  ios_iap_enabled: true
};
const ids = appleProductIds("dev")!;
const pro: StoreKitProduct = {
  productId: ids.Pro,
  title: "Maple Pro",
  description: "Pro monthly",
  formattedPrice: "23,00 €",
  priceCurrencyCode: "EUR",
  subscriptionPeriod: "P1M"
};

describe("iOS pricing policy", () => {
  test("legacy paid checkout requires successful current status, including after a cached response fails to refresh", () => {
    expect(hasConfirmedBillingStatus(undefined, false, false)).toBe(false);
    expect(hasConfirmedBillingStatus(undefined, false, true)).toBe(false);
    expect(hasConfirmedBillingStatus(free, true, false)).toBe(false);
    expect(hasConfirmedBillingStatus(free, false, true)).toBe(false);
    expect(hasConfirmedBillingStatus(free, false, false)).toBe(true);
    expect(hasConfirmedBillingStatus({ ...free, payment_provider: "apple" }, false, true)).toBe(
      false
    );
  });
  test("separates the app records and fails closed for an unknown variant", () => {
    expect(ids.Pro).toBe("cloud.opensecret.maple.dev.pro.monthly");
    expect(appleProductIds(undefined)?.Max).toBe("cloud.opensecret.maple.max.monthly");
    expect(appleProductIds("production")).toEqual(appleProductIds(undefined));
    expect(appleProductIds("preview")).toBeNull();
  });
  test("retains localized Apple prices, excludes wrong app, period, missing price and duplicate metadata", () => {
    expect(monthlyAppleProducts([pro], ids).Pro?.formattedPrice).toBe("23,00 €");
    for (const invalid of [
      { ...pro, productId: appleProductIds("production")!.Pro },
      { ...pro, subscriptionPeriod: "P1Y" },
      { ...pro, subscriptionPeriod: null },
      { ...pro, formattedPrice: " " },
      { ...pro, priceCurrencyCode: "" }
    ])
      expect(monthlyAppleProducts([invalid], ids)).toEqual({});
    expect(monthlyAppleProducts([pro, pro], ids)).toEqual({});
    expect(monthlyAppleProducts([], ids)).toEqual({});
  });
  test("free accounts may buy, including when the legacy is_subscribed field is true", () => {
    expect(applePurchaseConflict(free)).toBeNull();
  });
  test("Team precedence blocks personal purchases before Apple is selected", () => {
    expect(
      applePurchaseConflict({ ...free, product_name: "Team", payment_provider: "stripe" })
    ).toContain("Team");
  });
  test("a still-billing losing provider blocks duplicate purchase and an ended one does not", () => {
    const status: BillingStatus = {
      ...free,
      product_name: "Pro",
      payment_provider: "apple",
      subscriptions: [
        { provider: "stripe", plan: "max", state: "past_due", renews_at: null, manage: "portal" }
      ]
    };
    expect(applePurchaseConflict(status)).toContain("another payment provider");
    for (const state of ["canceled", "incomplete_expired", "expired", "revoked"]) {
      expect(
        applePurchaseConflict({
          ...status,
          subscriptions: [{ ...status.subscriptions![0], state }]
        })
      ).toBeNull();
    }
  });
  for (const payment_provider of ["stripe", "zaprite", "subscription_pass"] as const) {
    test(`blocks overlapping ${payment_provider} access even without subscription details`, () => {
      expect(
        applePurchaseConflict({ ...free, product_name: "Max", payment_provider })
      ).not.toBeNull();
    });
  }
  test("only an entitled Apple plan gets Apple's manage action", () => {
    expect(
      currentApplePlan({ ...free, payment_provider: "apple", product_name: "Pro" }, "Pro")
    ).toBe(true);
    expect(
      currentApplePlan({ ...free, payment_provider: "stripe", product_name: "Pro" }, "Pro")
    ).toBe(false);
    expect(
      currentApplePlan(
        { ...free, payment_provider: "apple", product_name: "Pro", is_subscribed: false },
        "Pro"
      )
    ).toBe(false);
  });
  test("web, desktop and Android must manage Apple access before buying from another provider", () => {
    expect(
      hasAppleSubscriptionToManage({ ...free, payment_provider: "apple", product_name: "Pro" })
    ).toBe(true);
    for (const state of ["active", "grace", "billing_retry"]) {
      expect(
        hasAppleSubscriptionToManage({
          ...free,
          subscriptions: [
            { provider: "apple", plan: "Pro", state, renews_at: null, manage: "app_store" }
          ]
        })
      ).toBe(true);
    }
    for (const payment_provider of [null, "stripe", "zaprite", "subscription_pass"] as const) {
      expect(hasAppleSubscriptionToManage({ ...free, payment_provider })).toBe(false);
    }
    for (const state of ["expired", "revoked", "superseded", "transfer_pending", "inactive"]) {
      expect(
        hasAppleSubscriptionToManage({
          ...free,
          subscriptions: [
            { provider: "apple", plan: "Pro", state, renews_at: null, manage: "app_store" }
          ]
        })
      ).toBe(false);
    }
  });

  // A plan billing lists beyond the paywall's two cards. The names are
  // fixtures: this app learns such a plan only from billing.
  const studioId = "app.example.studio.monthly";
  function listed(overrides: Partial<BillingProduct> & Pick<BillingProduct, "id" | "name">) {
    return {
      description: "",
      active: true,
      default_price: { id: "price", currency: "usd", unit_amount: 0, recurring: null },
      ...overrides
    } as BillingProduct;
  }
  const billingList: BillingProduct[] = [
    listed({
      id: "prod_pro",
      name: "Pro",
      plan_key: "pro",
      plan_group: "individual",
      plan_rank: 1,
      monthly_credits: 20_000,
      apple_product_id: ids.Pro
    }),
    listed({
      id: "prod_max",
      name: "Max",
      plan_key: "max",
      plan_group: "individual",
      plan_rank: 2,
      monthly_credits: 200_000,
      apple_product_id: ids.Max
    }),
    listed({
      id: "prod_studio",
      name: "Max Studio",
      plan_key: "studio",
      plan_group: "individual",
      plan_rank: 3,
      display_name: "Studio",
      monthly_credits: 400_000,
      apple_product_id: studioId
    }),
    listed({ id: "prod_team", name: "Team", plan_key: "team", plan_group: "team", plan_rank: null })
  ];
  const studio = appleSizes(billingList, ids).Max?.[0];

  test("a plan billing lists with an App Store product becomes a size of the plan below it", () => {
    expect(appleSizes(billingList, ids)).toEqual({
      Max: [
        {
          productId: studioId,
          planKey: "studio",
          catalogName: "Max Studio",
          displayName: "Studio",
          credits: "20× Pro's monthly credits"
        }
      ]
    });
    // Not a multiple of Pro: the allowance is stated as it is.
    const odd = billingList.map((product) =>
      product.id === "prod_studio" ? { ...product, monthly_credits: 250_000 } : product
    );
    expect(appleSizes(odd, ids).Max?.[0].credits).toBe("250,000 monthly credits");
  });

  test("nothing is offered without billing's list, its plan fields or an App Store product of its own", () => {
    expect(appleSizes(undefined, ids)).toEqual({});
    expect(appleSizes(billingList, null)).toEqual({});
    // An older billing server sends names only.
    expect(
      appleSizes(
        billingList.map(({ id, name, description, active, default_price }) => ({
          id,
          name,
          description,
          active,
          default_price
        })),
        ids
      )
    ).toEqual({});
    const withStudio = (change: Partial<BillingProduct>) =>
      billingList.map((product) =>
        product.id === "prod_studio" ? { ...product, ...change } : product
      );
    expect(appleSizes(withStudio({ apple_product_id: undefined }), ids)).toEqual({});
    // It may not reuse the identifier of the card it would sit on.
    expect(appleSizes(withStudio({ apple_product_id: ids.Max }), ids)).toEqual({});
    expect(appleSizes(withStudio({ active: false }), ids)).toEqual({});
    // A larger size needs its plan to be on sale.
    expect(
      appleSizes(
        billingList.filter((product) => product.id !== "prod_max" && product.id !== "prod_pro"),
        ids
      )
    ).toEqual({});
  });

  test("a size's App Store product passes the same checks as a plan's", () => {
    const valid: StoreKitProduct = { ...pro, productId: studioId, formattedPrice: "$229.99" };
    expect(monthlyAppleSizeProducts([pro, valid], [studioId])).toEqual({ [studioId]: valid });
    expect(monthlyAppleSizeProducts([pro], [studioId])).toEqual({});
    expect(monthlyAppleSizeProducts([valid, valid], [studioId])).toEqual({});
    expect(monthlyAppleSizeProducts([{ ...valid, subscriptionPeriod: "P1Y" }], [studioId])).toEqual(
      {}
    );
  });

  test("only an Apple subscriber on that plan is on a size", () => {
    const apple: BillingStatus = { ...free, payment_provider: "apple", product_name: "Max Studio" };
    expect(currentAppleSize({ ...apple, plan_key: "studio" }, studio!)).toBe(true);
    // An older billing server reports the name only.
    expect(currentAppleSize(apple, studio!)).toBe(true);
    expect(currentAppleSize({ ...apple, plan_key: "max", product_name: "Max" }, studio!)).toBe(
      false
    );
    expect(currentAppleSize({ ...apple, payment_provider: "stripe" }, studio!)).toBe(false);
    expect(currentAppleSize({ ...apple, is_subscribed: false }, studio!)).toBe(false);
    expect(currentAppleSize(undefined, studio!)).toBe(false);
    // The plan's own card is not current for a subscriber on the larger size.
    expect(currentApplePlan({ ...apple, plan_key: "studio" }, "Max")).toBe(false);
  });
});
