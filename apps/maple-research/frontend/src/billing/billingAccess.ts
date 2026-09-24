import type { BillingStatus, BillingSubscription } from "./billingApi";

export function isKnownFreePlan(billingStatus: BillingStatus | null | undefined): boolean {
  if (!billingStatus) return false;

  const productName = billingStatus.product_name?.trim().toLowerCase() ?? "";
  return productName === "free";
}

export function hasApiAccess(billingStatus: BillingStatus | null | undefined): boolean {
  const productName = billingStatus?.product_name?.toLowerCase() ?? "";

  return productName.includes("pro") || productName.includes("max") || productName.includes("team");
}

export function shouldWarnBeforeAccountDeletion(
  billingStatus: BillingStatus | null | undefined
): boolean {
  if (!billingStatus) return false;

  return (
    subscriptionsBeforeDeletion(billingStatus).length > 0 ||
    (!isKnownFreePlan(billingStatus) &&
      (billingStatus.is_subscribed || hasApiAccess(billingStatus)))
  );
}

function subscriptionsBeforeDeletion(status: BillingStatus): BillingSubscription[] {
  return (status.subscriptions ?? []).filter(
    ({ state }) => !["expired", "revoked", "superseded", "canceled"].includes(state)
  );
}

export function accountDeletionBillingDescription(
  status: BillingStatus | null | undefined
): string {
  const messages = [
    "Deleting your account ends your Maple access, including any remaining paid time."
  ];
  const subscriptions = status ? subscriptionsBeforeDeletion(status) : [];
  // Stripe-only responses can still omit the list. A customer ID alone does not
  // establish the current provider or ownership of a legacy Team subscription.
  const legacyProvider = status?.subscriptions === undefined ? status?.payment_provider : null;
  const apple = subscriptions.filter(({ provider }) => provider === "apple");
  const ownedCard = subscriptions.filter(
    ({ provider, manage }) => provider === "stripe" && manage === "portal"
  );

  if (apple.length > 0 || legacyProvider === "apple") {
    messages.push(
      apple.length > 0 && apple.every(({ auto_renew_enabled }) => auto_renew_enabled === false)
        ? "Your Apple subscription is already set not to renew. You can delete now without waiting for it to expire."
        : "Deleting your Maple account does not cancel Apple subscriptions. Cancel through Apple to stop future renewals. If you already canceled, you can continue without waiting for the subscription to expire."
    );
  }

  if (ownedCard.length > 0 || (legacyProvider === "stripe" && status?.stripe_customer_id)) {
    const ownsTeam = ownedCard.some(({ plan }) => plan.toLowerCase().includes("team"));
    const legacyTeam =
      legacyProvider === "stripe" && status?.product_name?.toLowerCase().includes("team");
    messages.push(
      ownsTeam
        ? "Your card subscriptions will be canceled automatically during deletion, including your team's subscription. Members will lose access provided by that team subscription."
        : legacyTeam
          ? "Any card subscriptions you own will be canceled automatically during deletion. If you own a team subscription, members will lose access provided by it."
          : "Your card subscriptions will be canceled automatically during deletion."
    );
  } else if (subscriptions.some(({ manage }) => manage === "team_admin")) {
    messages.push("Your team's subscription is managed by its owner and will not be canceled.");
  }

  if (
    messages.length === 1 &&
    legacyProvider !== "zaprite" &&
    legacyProvider !== "subscription_pass" &&
    !subscriptions.some(
      ({ provider }) => provider === "zaprite" || provider === "subscription_pass"
    )
  ) {
    messages.push(
      "Review your billing before continuing, and cancel any subscriptions that still renew."
    );
  }
  return messages.join(" ");
}
