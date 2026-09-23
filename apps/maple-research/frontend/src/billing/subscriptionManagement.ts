import type { BillingStatus, BillingSubscription } from "./billingApi";

export const APPLE_SUBSCRIPTIONS_URL = "https://apps.apple.com/account/subscriptions";

export type SubscriptionManagementAction = "apple" | "stripe" | "support" | null;

export function subscriptionManagementAction(
  subscription: BillingSubscription
): SubscriptionManagementAction {
  if (subscription.provider === "apple" && subscription.manage === "app_store") return "apple";
  if (subscription.provider === "stripe" && subscription.manage === "portal") return "stripe";
  if (subscription.provider === "zaprite" && subscription.manage === "zaprite") return "support";
  return null;
}

/** Older servers omit the list. Preserve their existing Stripe management path. */
export function subscriptionsForManagement(
  status: BillingStatus | null | undefined
): BillingSubscription[] {
  if (!status) return [];
  if (status.subscriptions !== undefined) return status.subscriptions;
  const paid = ["pro", "max", "team"].some((plan) =>
    status.product_name?.toLowerCase().includes(plan)
  );
  if (!paid) return [];
  if (status.payment_provider === "apple") {
    return [
      {
        provider: "apple",
        plan: status.product_name ?? "Apple subscription",
        state: status.subscription_status ?? "active",
        renews_at: billingPeriodSeconds(status.current_period_end),
        manage: "app_store",
        selected: true
      }
    ];
  }
  if (!status.stripe_customer_id) return [];
  return [
    {
      provider: "stripe",
      plan: status.product_name ?? "Subscription",
      state: status.subscription_status ?? "active",
      renews_at: billingPeriodSeconds(status.current_period_end),
      manage: "portal",
      selected: true
    }
  ];
}

function billingPeriodSeconds(value: BillingStatus["current_period_end"]): number | null {
  if (value === null || value === "") return null;
  const numeric = Number(value);
  if (Number.isFinite(numeric)) return numeric;
  const milliseconds = Date.parse(String(value));
  return Number.isFinite(milliseconds) ? milliseconds / 1000 : null;
}

export function billingPeriodDate(value: BillingStatus["current_period_end"]): string | null {
  const seconds = billingPeriodSeconds(value);
  if (seconds === null) return null;
  const date = new Date(seconds * 1000);
  if (!Number.isFinite(date.getTime())) return null;
  return date.toLocaleDateString(undefined, { year: "numeric", month: "long", day: "numeric" });
}

export function subscriptionProviderLabel(provider: string): string {
  switch (provider) {
    case "apple":
      return "Apple App Store";
    case "stripe":
      return "Card subscription";
    case "zaprite":
      return "Bitcoin subscription";
    case "subscription_pass":
      return "Subscription pass";
    default:
      return "Subscription";
  }
}

export function subscriptionStateLabel(state: string): string {
  switch (state) {
    case "active":
      return "Active";
    case "trialing":
      return "Trial";
    case "grace":
      return "Payment grace period";
    case "billing_retry":
    case "past_due":
    case "unpaid":
      return "Payment needs attention";
    case "expired":
      return "Expired";
    case "revoked":
      return "Revoked";
    case "superseded":
      return "Replaced by another plan";
    case "transfer_pending":
      return "Transfer pending";
    case "owner_retired":
      return "Account transfer required";
    case "canceled":
      return "Canceled";
    default:
      return "Check subscription status";
  }
}

export function subscriptionPeriodLabel(subscription: BillingSubscription): string {
  if (["expired", "revoked", "superseded", "canceled"].includes(subscription.state)) {
    return "Ended";
  }
  if (subscription.state === "grace" || subscription.state === "billing_retry") {
    return "Paid period ended";
  }
  if (
    subscription.provider === "zaprite" ||
    subscription.provider === "subscription_pass" ||
    subscription.auto_renew_enabled === false
  ) {
    return "Expires";
  }
  return "Current period ends";
}
