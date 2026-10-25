import { useQuery } from "@tanstack/react-query";
import type { BillingStatus } from "./billingApi";
import { getBillingService } from "./billingService";
import { hasAppleSubscriptionToManage } from "./applePricingPolicy";
import { isKnownFreePlan } from "./billingAccess";
import { subscriptionManagementAction } from "./subscriptionManagement";
import { assertChatAccountCredential } from "@/services/chatAccountCredential";

export type PricingPortalInput = {
  userId: string | undefined;
  status: BillingStatus | undefined;
  statusConfirmed: boolean;
};

async function requestPortalForAccount(userId: string): Promise<string> {
  assertChatAccountCredential(userId);
  const url = await getBillingService().getPortalUrl();
  assertChatAccountCredential(userId);
  return url;
}

export function usePricingPortal(
  { userId, status, statusConfirmed }: PricingPortalInput,
  requestPortal: (userId: string) => Promise<string> = requestPortalForAccount
) {
  // Respect explicit management hints. Older servers can omit the list; keep
  // their existing Stripe customer path without restricting legacy plan names.
  const hasStripeManagement =
    status?.subscriptions !== undefined
      ? status.subscriptions.some(
          (subscription) => subscriptionManagementAction(subscription) === "stripe"
        )
      : !!status?.stripe_customer_id && !isKnownFreePlan(status);

  const enabled =
    !!userId &&
    statusConfirmed &&
    status?.payment_provider !== "zaprite" &&
    status?.payment_provider !== "subscription_pass" &&
    !hasAppleSubscriptionToManage(status) &&
    hasStripeManagement;

  const query = useQuery({
    queryKey: ["portalUrl", userId],
    queryFn: async () => {
      if (!userId) return null;
      try {
        return await requestPortal(userId);
      } catch (error) {
        console.error("Error fetching portal URL:", error);
        return null;
      }
    },
    enabled
  });

  return {
    portalUrl: query.data,
    // A cached failed request can contain null while its replacement is pending.
    isAwaitingPortal: enabled && query.isFetching && !query.data
  };
}
