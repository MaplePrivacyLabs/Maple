import { describe, expect, test } from "bun:test";
import type { BillingStatus } from "./billingApi";
import {
  appleProductIds,
  applePurchaseConflict,
  currentApplePlan,
  hasAppleSubscriptionToManage,
  hasConfirmedBillingStatus,
  monthlyAppleProducts
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
});
