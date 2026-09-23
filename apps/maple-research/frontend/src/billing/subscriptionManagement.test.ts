import { describe, expect, mock, test } from "bun:test";
import type { BillingStatus, BillingSubscription } from "./billingApi";
import { openAppleSubscriptionManagement } from "./appleSubscriptionManagement";
import {
  APPLE_SUBSCRIPTIONS_URL,
  billingPeriodDate,
  subscriptionManagementAction,
  subscriptionPeriodLabel,
  subscriptionsForManagement
} from "./subscriptionManagement";

const baseStatus: BillingStatus = {
  is_subscribed: true,
  stripe_customer_id: "cus_fixture",
  product_id: "max",
  product_name: "Max",
  subscription_status: "active",
  current_period_end: 1_790_812_800,
  can_chat: true,
  chats_remaining: null,
  payment_provider: "stripe",
  total_tokens: 100,
  used_tokens: 0,
  usage_reset_date: null
};
const apple: BillingSubscription = {
  provider: "apple",
  plan: "Pro",
  state: "active",
  renews_at: 1_790_812_800,
  manage: "app_store",
  selected: false,
  environment: "Sandbox",
  auto_renew_enabled: false
};

describe("subscription management", () => {
  test("shows every row even when a different provider supplies access and purchases are off", () => {
    const subscriptions = [
      { ...apple, provider: "stripe", plan: "Max", manage: "portal", selected: true },
      apple,
      { ...apple, environment: "Production", state: "expired" }
    ];
    expect(
      subscriptionsForManagement({ ...baseStatus, subscriptions, ios_iap_enabled: false })
    ).toEqual(subscriptions);
  });

  test("preserves legacy Stripe management but never creates one over the authoritative list", () => {
    expect(subscriptionsForManagement(baseStatus)[0]?.manage).toBe("portal");
    expect(subscriptionsForManagement({ ...baseStatus, subscriptions: [] })).toEqual([]);
    expect(subscriptionsForManagement({ ...baseStatus, stripe_customer_id: null })).toEqual([]);
    expect(subscriptionsForManagement({ ...baseStatus, product_name: "Free" })).toEqual([]);
    expect(subscriptionsForManagement(null)).toEqual([]);
  });

  test("keeps Apple management available if the server omits management hints", () => {
    expect(
      subscriptionsForManagement({
        ...baseStatus,
        payment_provider: "apple",
        stripe_customer_id: null
      })
    ).toEqual([
      {
        provider: "apple",
        plan: "Max",
        state: "active",
        renews_at: 1_790_812_800,
        manage: "app_store",
        selected: true
      }
    ]);
  });

  test("does not turn unknown hints, URLs, or team member management into privileged actions", () => {
    expect(subscriptionManagementAction(apple)).toBe("apple");
    expect(subscriptionManagementAction({ ...apple, provider: "stripe", manage: "portal" })).toBe(
      "stripe"
    );
    for (const manage of [
      "team_admin",
      "zaprite",
      "subscription_pass",
      "https://evil.example",
      "portal"
    ]) {
      expect(subscriptionManagementAction({ ...apple, manage })).toBeNull();
    }
    expect(
      subscriptionManagementAction({ ...apple, provider: "stripe", manage: "team_admin" })
    ).toBeNull();
    expect(subscriptionManagementAction({ ...apple, provider: "zaprite", manage: "zaprite" })).toBe(
      "support"
    );
    expect(subscriptionManagementAction({ ...apple, provider: "unknown" })).toBeNull();
  });

  test("does not promise renewal for canceled auto-renew or past periods", () => {
    expect(subscriptionPeriodLabel(apple)).toBe("Expires");
    expect(subscriptionPeriodLabel({ ...apple, auto_renew_enabled: true })).toBe(
      "Current period ends"
    );
    expect(subscriptionPeriodLabel({ ...apple, state: "expired" })).toBe("Ended");
    expect(subscriptionPeriodLabel({ ...apple, state: "grace" })).toBe("Paid period ended");
  });

  test("accepts wire seconds and legacy timestamp strings without displaying Invalid Date", () => {
    const expected = new Date(1_790_812_800_000).toLocaleDateString(undefined, {
      year: "numeric",
      month: "long",
      day: "numeric"
    });
    expect(billingPeriodDate(1_790_812_800)).toBe(expected);
    expect(billingPeriodDate("1790812800")).toBe(expected);
    expect(billingPeriodDate(new Date(1_790_812_800_000).toISOString())).toBe(expected);
    expect(billingPeriodDate("not a date")).toBeNull();
    expect(billingPeriodDate(null)).toBeNull();
    expect(billingPeriodDate(1e100)).toBeNull();
  });

  test("uses the native subscription sheet only on iOS and a fixed Apple URL elsewhere", async () => {
    for (const platform of ["ios", "android", "macos", "windows", "linux", "web"]) {
      const manageNative = mock(async () => {});
      const openNativeUrl = mock<(url: string) => Promise<void>>(async () => {});
      const openWebUrl = mock<(url: string) => void>(() => {});
      await openAppleSubscriptionManagement({
        isIOS: () => platform === "ios",
        isTauri: () => platform !== "web",
        manageNative,
        openNativeUrl,
        openWebUrl
      });
      expect(manageNative).toHaveBeenCalledTimes(platform === "ios" ? 1 : 0);
      expect(openNativeUrl).toHaveBeenCalledTimes(platform !== "ios" && platform !== "web" ? 1 : 0);
      expect(openWebUrl).toHaveBeenCalledTimes(platform === "web" ? 1 : 0);
      if (platform === "web") expect(openWebUrl).toHaveBeenCalledWith(APPLE_SUBSCRIPTIONS_URL);
      if (platform !== "ios" && platform !== "web") {
        expect(openNativeUrl).toHaveBeenCalledWith(APPLE_SUBSCRIPTIONS_URL);
      }
    }
  });

  test("propagates native failures so the settings page can offer retry", async () => {
    await expect(
      openAppleSubscriptionManagement({
        isIOS: () => true,
        isTauri: () => true,
        manageNative: async () => {
          throw new Error("storekit_management_unavailable");
        },
        openNativeUrl: async () => {},
        openWebUrl: () => {}
      })
    ).rejects.toThrow("storekit_management_unavailable");
  });
});
