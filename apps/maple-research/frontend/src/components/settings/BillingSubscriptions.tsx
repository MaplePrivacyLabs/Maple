import { useState } from "react";
import { Loader2 } from "lucide-react";
import type { BillingStatus } from "@/billing/billingApi";
import { openAppleSubscriptionManagement } from "@/billing/appleSubscriptionManagement";
import { openBillingPortal } from "@/billing/billingPortal";
import {
  billingPeriodDate,
  subscriptionManagementAction,
  subscriptionPeriodLabel,
  subscriptionProviderLabel,
  subscriptionsForManagement,
  subscriptionStateLabel
} from "@/billing/subscriptionManagement";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { openExternalUrl } from "@/utils/openUrl";
import { SettingsSection } from "./SettingsPage";

type Props = {
  status: BillingStatus | null | undefined;
  manageApple?: () => Promise<void>;
  manageStripe?: () => Promise<void>;
  manageSupport?: () => Promise<void>;
};

/** Display every provider, including subscriptions that do not supply the current quota. */
export function BillingSubscriptions({
  status,
  manageApple = openAppleSubscriptionManagement,
  manageStripe = openBillingPortal,
  manageSupport = () => openExternalUrl("mailto:support@trymaple.ai")
}: Props) {
  const [opening, setOpening] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const subscriptions = subscriptionsForManagement(status);
  if (subscriptions.length === 0) return null;

  const openManagement = async (index: number) => {
    const action = subscriptionManagementAction(subscriptions[index]);
    if (action === null || opening !== null) return;
    setOpening(index);
    setError(null);
    try {
      await (action === "apple"
        ? manageApple()
        : action === "stripe"
          ? manageStripe()
          : manageSupport());
    } catch {
      setError("Unable to open subscription management. Please try again.");
    } finally {
      setOpening(null);
    }
  };

  return (
    <SettingsSection title="Subscriptions">
      {status?.conflict && (
        <Alert className="mb-4">
          <AlertDescription>
            You have subscriptions with more than one provider. Changing your Maple plan does not
            cancel another provider&apos;s subscription. Review each subscription below.
          </AlertDescription>
        </Alert>
      )}
      <ul className="divide-y divide-border">
        {subscriptions.map((subscription, index) => {
          const action = subscriptionManagementAction(subscription);
          const endDate = billingPeriodDate(subscription.renews_at);
          return (
            <li
              key={`${subscription.provider}:${subscription.product_id ?? subscription.plan}:${subscription.environment ?? ""}:${index}`}
              className="flex flex-col gap-3 py-4 first:pt-0 last:pb-0 sm:flex-row sm:items-center sm:justify-between"
            >
              <div className="min-w-0">
                <p className="font-medium">
                  {subscription.plan} — {subscriptionProviderLabel(subscription.provider)}
                </p>
                <p className="text-sm text-muted-foreground">
                  {subscriptionStateLabel(subscription.state)}
                  {subscription.selected && " · Supplies your current plan"}
                  {subscription.environment === "Sandbox" && " · Apple sandbox test"}
                </p>
                {endDate && (
                  <p className="text-sm text-muted-foreground">
                    {subscriptionPeriodLabel(subscription)} on {endDate}
                  </p>
                )}
                {subscription.manage === "team_admin" && (
                  <p className="text-sm text-muted-foreground">
                    Your team administrator manages this subscription.
                  </p>
                )}
                {subscription.provider === "zaprite" && (
                  <p className="text-sm text-muted-foreground">
                    Paid with Bitcoin. This subscription does not renew automatically.
                  </p>
                )}
                {subscription.provider === "subscription_pass" && (
                  <p className="text-sm text-muted-foreground">
                    This pass does not renew automatically.
                  </p>
                )}
              </div>
              {action && (
                <Button
                  type="button"
                  variant="outline"
                  onClick={() => void openManagement(index)}
                  disabled={opening !== null}
                >
                  {opening === index && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                  {opening === index
                    ? "Opening..."
                    : action === "apple"
                      ? "Manage with Apple"
                      : action === "stripe"
                        ? "Manage card subscription"
                        : "Contact support"}
                </Button>
              )}
            </li>
          );
        })}
      </ul>
      {error && (
        <Alert variant="destructive" className="mt-4">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}
    </SettingsSection>
  );
}
