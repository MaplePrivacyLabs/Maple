import type { BillingStatus } from "./billingApi";
import type { StoreKitProduct } from "@/services/storeKitService";

export type ApplePlan = "Pro" | "Max";

// App Store product identifiers are public and belong to distinct app records.
// The legacy /products API contains Stripe prices, not an Apple catalog.
export function appleProductIds(variant: string | undefined): Record<ApplePlan, string> | null {
  if (variant !== undefined && variant !== "production" && variant !== "dev") return null;
  const bundle = variant === "dev" ? "cloud.opensecret.maple.dev" : "cloud.opensecret.maple";
  return { Pro: `${bundle}.pro.monthly`, Max: `${bundle}.max.monthly` };
}

export function monthlyAppleProducts(
  products: readonly StoreKitProduct[],
  ids: Record<ApplePlan, string>
): Partial<Record<ApplePlan, StoreKitProduct>> {
  const result: Partial<Record<ApplePlan, StoreKitProduct>> = {};
  for (const plan of ["Pro", "Max"] as const) {
    const matches = products.filter((product) => product.productId === ids[plan]);
    const product = matches.length === 1 ? matches[0] : undefined;
    if (
      product?.subscriptionPeriod === "P1M" &&
      product.formattedPrice.trim() &&
      /^[A-Z]{3}$/.test(product.priceCurrencyCode)
    ) {
      result[plan] = product;
    }
  }
  return result;
}

const endedStates = new Set([
  "canceled",
  "incomplete_expired",
  "expired",
  "revoked",
  "superseded",
  "inactive"
]);

/** Presentation guard against duplicate subscriptions; the server owns entitlement. */
export function applePurchaseConflict(status: BillingStatus | undefined): string | null {
  if (!status) return null;
  if (status.product_name?.toLowerCase().includes("team")) {
    return "Your Team plan already includes Maple access. Manage your Team subscription before buying a personal plan.";
  }
  if (
    status.subscriptions?.some(
      (subscription) => subscription.provider !== "apple" && !endedStates.has(subscription.state)
    ) ||
    (status.payment_provider !== null &&
      status.payment_provider !== "apple" &&
      status.is_subscribed &&
      status.product_name?.toLowerCase() !== "free") ||
    status.conflict
  ) {
    return "You already have access or a subscription from another payment provider. Review your billing before starting an Apple subscription to avoid overlapping plans.";
  }
  return null;
}

export function currentApplePlan(status: BillingStatus | undefined, plan: ApplePlan): boolean {
  return (
    status?.payment_provider === "apple" &&
    status.is_subscribed &&
    status.product_name?.toLowerCase() === plan.toLowerCase()
  );
}

/** Prevent cross-provider checkout when Apple supplies access or may renew again. */
export function hasAppleSubscriptionToManage(status: BillingStatus | undefined): boolean {
  return (
    (status?.payment_provider === "apple" && status.is_subscribed) ||
    status?.subscriptions?.some(
      (subscription) =>
        subscription.provider === "apple" &&
        ["active", "grace", "billing_retry"].includes(subscription.state)
    ) === true
  );
}

export function hasConfirmedBillingStatus(
  status: BillingStatus | undefined,
  loading: boolean,
  failed: boolean
): status is BillingStatus {
  // A stale cached Free response must not permit another checkout after a
  // current status request fails (the account may now have an Apple plan).
  return status !== undefined && !loading && !failed;
}
