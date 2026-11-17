import type { BillingProduct, BillingStatus } from "./billingApi";
import { productDisplayName, productsByCard, type PlanCard } from "./plan";
import type { StoreKitProduct } from "@/services/storeKitService";

export type ApplePlan = "Pro" | "Max";

/**
 * A larger size of one of the paywall's plans. Billing names its App Store
 * product, so a plan added later is offered without an identifier here.
 */
export type AppleSize = {
  productId: string;
  planKey: string | undefined;
  /** Billing's `name` for the plan: what `product_name` reports for a subscriber. */
  catalogName: string;
  displayName: string;
  /** The plan's allowance in the paywall's wording, when billing gave it. */
  credits: string | null;
};

/** A size the App Store returned a valid monthly product for. */
export type AppleSizeOffer = AppleSize & { product: StoreKitProduct };

const paywallCards: readonly (PlanCard & { name: ApplePlan })[] = [
  { name: "Pro", planKey: "pro", rank: 1 },
  { name: "Max", planKey: "max", rank: 2 }
];

// App Store product identifiers are public and belong to distinct app records.
// The legacy /products API contains Stripe prices, not an Apple catalog.
export function appleProductIds(variant: string | undefined): Record<ApplePlan, string> | null {
  if (variant !== undefined && variant !== "production" && variant !== "dev") return null;
  const bundle = variant === "dev" ? "cloud.opensecret.maple.dev" : "cloud.opensecret.maple";
  return { Pro: `${bundle}.pro.monthly`, Max: `${bundle}.max.monthly` };
}

function monthlyProduct(
  products: readonly StoreKitProduct[],
  productId: string
): StoreKitProduct | undefined {
  const matches = products.filter((product) => product.productId === productId);
  const product = matches.length === 1 ? matches[0] : undefined;
  return product?.subscriptionPeriod === "P1M" &&
    product.formattedPrice.trim() &&
    /^[A-Z]{3}$/.test(product.priceCurrencyCode)
    ? product
    : undefined;
}

export function monthlyAppleProducts(
  products: readonly StoreKitProduct[],
  ids: Record<ApplePlan, string>
): Partial<Record<ApplePlan, StoreKitProduct>> {
  const result: Partial<Record<ApplePlan, StoreKitProduct>> = {};
  for (const plan of ["Pro", "Max"] as const) {
    const product = monthlyProduct(products, ids[plan]);
    if (product) result[plan] = product;
  }
  return result;
}

/** The same validation for the sizes billing lists, keyed by App Store product. */
export function monthlyAppleSizeProducts(
  products: readonly StoreKitProduct[],
  productIds: readonly string[]
): Record<string, StoreKitProduct> {
  const result: Record<string, StoreKitProduct> = {};
  for (const productId of productIds) {
    const product = monthlyProduct(products, productId);
    if (product) result[productId] = product;
  }
  return result;
}

function sizeCredits(product: BillingProduct, proCredits: number | undefined): string | null {
  const credits = product.monthly_credits;
  if (typeof credits !== "number" || credits <= 0) return null;
  const multiple = proCredits && credits % proCredits === 0 ? credits / proCredits : 0;
  return multiple > 1
    ? `${multiple}× Pro's monthly credits`
    : `${credits.toLocaleString("en-US")} monthly credits`;
}

/**
 * The larger sizes billing lists for the paywall's plans. A plan with an App
 * Store product and no card of its own is offered by the highest plan below
 * it. Pro and Max keep their fixed identifiers: the paywall has to work when
 * the product list cannot be loaded.
 */
export function appleSizes(
  products: readonly BillingProduct[] | undefined,
  ids: Record<ApplePlan, string> | null
): Partial<Record<ApplePlan, AppleSize[]>> {
  if (!products || !ids) return {};
  const taken = new Set<string>(Object.values(ids));
  const proCredits = products.find((product) => product.plan_key === "pro")?.monthly_credits;
  const result: Partial<Record<ApplePlan, AppleSize[]>> = {};
  const offered = productsByCard(paywallCards, products);
  for (const card of paywallCards) {
    const sizes: AppleSize[] = [];
    for (const product of offered.get(card.name) ?? []) {
      const own = product.plan_key
        ? product.plan_key === card.planKey
        : product.name.trim().toLowerCase() === card.name.toLowerCase();
      const productId = product.apple_product_id?.trim();
      if (own || !productId || taken.has(productId)) continue;
      taken.add(productId);
      sizes.push({
        productId,
        planKey: product.plan_key,
        catalogName: product.name,
        displayName: productDisplayName(product),
        credits: sizeCredits(product, proCredits)
      });
    }
    if (sizes.length) result[card.name] = sizes;
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

export function currentAppleSize(status: BillingStatus | undefined, size: AppleSize): boolean {
  if (status?.payment_provider !== "apple" || !status.is_subscribed) return false;
  if (status.plan_key && size.planKey) return status.plan_key === size.planKey;
  return status.product_name?.trim().toLowerCase() === size.catalogName.trim().toLowerCase();
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
