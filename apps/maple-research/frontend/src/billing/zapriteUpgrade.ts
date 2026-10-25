import type { BillingProduct, BillingStatus } from "./billingApi";

export const ZAPRITE_UPGRADE_TERMINAL_STATUSES = [
  "COMPLETED",
  "EXPIRED",
  "CANCELED",
  "FAILED",
  "REFUNDED",
  "REVOKED",
  "PAID_UNFULFILLABLE"
] as const;

export type ZapriteUpgradeStatus =
  | "QUOTED"
  | "CREATING_ORDER"
  | "PENDING"
  | "PROCESSING"
  | "PAID"
  | "UNDERPAID"
  | "OVERPAID"
  | "COMPLETED"
  | "EXPIRED"
  | "CANCELED"
  | "FAILED"
  | "PAID_NEEDS_REVIEW"
  | "PAID_UNFULFILLABLE"
  | "REFUNDED"
  | "REVOKED";

export type ZapriteUpgradePlanSummary = {
  product_id: string;
  plan_name: string;
  monthly_amount_cents?: number;
  annual_amount_cents: number;
  discount_basis_points?: number;
};

export type ZapriteUpgradeQuote = {
  quote_id: string;
  source: ZapriteUpgradePlanSummary;
  target: ZapriteUpgradePlanSummary;
  subscription_start: string;
  subscription_end: string;
  quote_effective_at: string;
  period_seconds: number;
  remaining_seconds: number;
  amount_due_cents: number;
  currency: string;
  expires_at: string;
};

export type ZapriteUpgradeCreateResponse = {
  upgrade_id: string;
  status: ZapriteUpgradeStatus;
  checkout_url: string;
  quote: ZapriteUpgradeQuote;
};

export type ZapriteUpgradeStatusResponse = {
  upgrade_id: string;
  status: ZapriteUpgradeStatus;
  provider_status?: string;
  quote: ZapriteUpgradeQuote;
  checkout_url?: string;
  completed_at?: string;
};

export type BillingApiError = {
  error: string;
  code?: string;
};

export function isZapriteUpgradeTerminal(status: string): boolean {
  return (ZAPRITE_UPGRADE_TERMINAL_STATUSES as readonly string[]).includes(status);
}

export function formatUsdFromCents(cents: number): string {
  return new Intl.NumberFormat("en-US", {
    style: "currency",
    currency: "USD"
  }).format(cents / 100);
}

export function isAllowedZapriteCheckoutUrl(url: string): boolean {
  try {
    const parsed = new URL(url);
    const host = parsed.hostname;
    if (
      parsed.protocol === "https:" &&
      (host === "checkout.zaprite.com" || host.endsWith(".zaprite.com"))
    ) {
      return true;
    }
    return (
      parsed.protocol === "http:" &&
      (host === "127.0.0.1" || host === "localhost" || host === "0.0.0.0")
    );
  } catch {
    return false;
  }
}

export function zapriteUpgradeTarget(
  billingStatus: BillingStatus | null | undefined,
  product: Pick<BillingProduct, "id" | "name">
): boolean {
  if (billingStatus?.payment_provider !== "zaprite") {
    return false;
  }
  const source = billingStatus.product_name?.trim().toLowerCase() ?? "";
  return source === "pro" && product.name.trim().toLowerCase() === "max";
}

export function zapritePaidPlanButtonText(
  billingStatus: BillingStatus | null | undefined,
  product: Pick<BillingProduct, "id" | "name">,
  isCurrentPlan: boolean
): "Upgrade to Max" | "Contact Us" | "Start Chatting" | null {
  if (billingStatus?.payment_provider !== "zaprite") {
    return null;
  }
  if (isCurrentPlan) {
    return "Start Chatting";
  }
  if (zapriteUpgradeTarget(billingStatus, product)) {
    return "Upgrade to Max";
  }
  return "Contact Us";
}

export function parseBillingApiError(errorText: string): BillingApiError {
  try {
    const parsed = JSON.parse(errorText) as BillingApiError;
    if (parsed && typeof parsed.error === "string") {
      return parsed;
    }
  } catch {
    // Fall through to the raw text.
  }
  return { error: errorText };
}
