import { describe, expect, test } from "bun:test";
import type { BillingStatus, BillingSubscription } from "./billingApi";
import {
  accountDeletionBillingDescription,
  hasApiAccess,
  isKnownFreePlan,
  shouldWarnBeforeAccountDeletion
} from "./billingAccess";

function billingStatus(productName: string): BillingStatus {
  return {
    is_subscribed: productName !== "Free",
    stripe_customer_id: null,
    product_id: "test-product",
    product_name: productName,
    subscription_status: "active",
    current_period_end: null,
    can_chat: true,
    chats_remaining: null,
    payment_provider: "stripe",
    total_tokens: null,
    used_tokens: null,
    usage_reset_date: null
  };
}

describe("hasApiAccess", () => {
  test.each(["Pro", "Max", "Team"])("allows the %s plan", (productName) => {
    expect(hasApiAccess(billingStatus(productName))).toBe(true);
  });

  test.each([null, undefined])("fails closed when billing is %s", (status) => {
    expect(hasApiAccess(status)).toBe(false);
  });

  test.each(["Free", "Unknown"])("does not allow the %s plan", (productName) => {
    expect(hasApiAccess(billingStatus(productName))).toBe(false);
  });
});

describe("isKnownFreePlan", () => {
  test("identifies a loaded free plan without treating unknown billing as free", () => {
    expect(isKnownFreePlan(billingStatus("Free"))).toBe(true);
    expect(isKnownFreePlan(billingStatus(""))).toBe(false);
    expect(isKnownFreePlan(billingStatus("Unknown"))).toBe(false);
    expect(isKnownFreePlan(billingStatus("Pro"))).toBe(false);
    expect(isKnownFreePlan(null)).toBe(false);
  });
});

describe("shouldWarnBeforeAccountDeletion", () => {
  test.each(["Pro", "Max", "Team"])("warns for the %s plan", (productName) => {
    expect(shouldWarnBeforeAccountDeletion(billingStatus(productName))).toBe(true);
  });

  test("does not warn for a free plan", () => {
    expect(shouldWarnBeforeAccountDeletion(billingStatus("Free"))).toBe(false);
    // Billing's free-tier response can still set is_subscribed to true.
    expect(shouldWarnBeforeAccountDeletion({ ...billingStatus("Free"), is_subscribed: true })).toBe(
      false
    );
  });

  test.each([null, undefined])("does not warn when billing is %s", (status) => {
    expect(shouldWarnBeforeAccountDeletion(status)).toBe(false);
  });

  test("warns when subscribed even if the product name is unrecognized", () => {
    expect(shouldWarnBeforeAccountDeletion(billingStatus("Unknown"))).toBe(true);
  });

  test("warns for a paid product name even if is_subscribed is false", () => {
    expect(
      shouldWarnBeforeAccountDeletion({
        ...billingStatus("Pro"),
        is_subscribed: false
      })
    ).toBe(true);
  });
});

function appleSubscription(overrides: Partial<BillingSubscription> = {}): BillingSubscription {
  return {
    provider: "apple",
    plan: "Pro",
    state: "active",
    manage: "app_store",
    renews_at: null,
    selected: false,
    ...overrides
  };
}

describe("accountDeletionBillingDescription", () => {
  test.each([true, null, undefined])(
    "does not assume Apple is canceled when renewal is %s",
    (renewal) => {
      const status = {
        ...billingStatus("Pro"),
        subscriptions: [appleSubscription({ auto_renew_enabled: renewal })]
      };
      expect(accountDeletionBillingDescription(status)).toContain(
        "does not cancel Apple subscriptions"
      );
      expect(accountDeletionBillingDescription(status)).not.toContain("already set not to renew");
    }
  );

  test("allows an Apple subscriber with renewal off to continue during the paid period", () => {
    const status = {
      ...billingStatus("Pro"),
      subscriptions: [appleSubscription({ auto_renew_enabled: false })]
    };
    expect(shouldWarnBeforeAccountDeletion(status)).toBe(true);
    expect(accountDeletionBillingDescription(status)).toContain("already set not to renew");
    expect(accountDeletionBillingDescription(status)).toContain("without waiting for it to expire");
  });

  test("covers unselected Apple billing alongside an owned card subscription", () => {
    const status = {
      ...billingStatus("Max"),
      subscriptions: [
        appleSubscription({ auto_renew_enabled: false }),
        appleSubscription({ auto_renew_enabled: true }),
        {
          provider: "stripe",
          plan: "Max",
          state: "active",
          manage: "portal",
          renews_at: null,
          selected: true
        }
      ]
    };
    const description = accountDeletionBillingDescription(status);
    expect(description).toContain("does not cancel Apple subscriptions");
    expect(description).toContain("card subscriptions will be canceled automatically");
    expect(description).not.toContain("already set not to renew");
  });

  test("handles Stripe-only responses that omit the subscription list", () => {
    expect(
      accountDeletionBillingDescription({
        ...billingStatus("Pro"),
        stripe_customer_id: "cus_fixture"
      })
    ).toContain("card subscriptions will be canceled automatically");
    expect(accountDeletionBillingDescription(billingStatus("Team"))).not.toContain(
      "will be canceled automatically"
    );
  });

  test("only asserts Team ownership when the subscription row identifies it", () => {
    const legacy = { ...billingStatus("Team"), stripe_customer_id: "cus_fixture" };
    const listed = {
      ...legacy,
      subscriptions: [
        { provider: "stripe", plan: "Team", state: "active", manage: "portal", renews_at: null }
      ]
    };
    expect(accountDeletionBillingDescription(listed)).toContain(
      "including your team's subscription"
    );
    expect(accountDeletionBillingDescription(listed)).toContain(
      "access provided by that team subscription"
    );
    // A legacy Team response may expose the admin's customer ID to a member.
    expect(accountDeletionBillingDescription(legacy)).toContain("If you own a team subscription");
    expect(accountDeletionBillingDescription(legacy)).not.toContain(
      "including your team's subscription"
    );
  });

  test("does not promise cancellation of a Team member's admin-owned subscription", () => {
    const status = {
      ...billingStatus("Team"),
      stripe_customer_id: "cus_old_personal",
      subscriptions: [
        {
          provider: "stripe",
          plan: "Team",
          state: "active",
          manage: "team_admin",
          renews_at: null
        },
        appleSubscription()
      ]
    };
    const description = accountDeletionBillingDescription(status);
    expect(description).toContain("managed by its owner and will not be canceled");
    expect(description).toContain("does not cancel Apple subscriptions");
    expect(description).not.toContain("will be canceled automatically");
  });

  test.each(["zaprite", "subscription_pass"] as const)(
    "does not mistake %s for a card plan because of an old customer ID",
    (provider) => {
      const status = {
        ...billingStatus("Pro"),
        payment_provider: provider,
        stripe_customer_id: "cus_old"
      };
      expect(accountDeletionBillingDescription(status)).toContain("remaining paid time");
      expect(accountDeletionBillingDescription(status)).not.toContain("cancel");
    }
  );

  test("keeps unknown and legacy Apple responses conservative", () => {
    expect(
      accountDeletionBillingDescription({ ...billingStatus("Pro"), payment_provider: "apple" })
    ).toContain("does not cancel Apple subscriptions");
    expect(
      accountDeletionBillingDescription({ ...billingStatus("Unknown"), payment_provider: null })
    ).toContain("Review your billing");
  });

  test.each(["expired", "revoked", "superseded", "canceled"])(
    "ignores historical %s Apple rows",
    (state) => {
      const status = {
        ...billingStatus("Free"),
        is_subscribed: true,
        subscriptions: [appleSubscription({ state })]
      };
      expect(shouldWarnBeforeAccountDeletion(status)).toBe(false);
      expect(accountDeletionBillingDescription(status)).not.toContain("Apple");
    }
  );

  test.each(["active", "grace", "billing_retry", "transfer_pending", "owner_retired", "unknown"])(
    "still warns about %s Apple billing even when Free is selected",
    (state) => {
      const status = { ...billingStatus("Free"), subscriptions: [appleSubscription({ state })] };
      expect(shouldWarnBeforeAccountDeletion(status)).toBe(true);
      expect(accountDeletionBillingDescription(status)).toContain(
        "does not cancel Apple subscriptions"
      );
    }
  );
});
