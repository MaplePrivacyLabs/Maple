import { Link } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import { CreditCard, KeyRound, Sparkles } from "lucide-react";
import { billingPeriodDate } from "@/billing/subscriptionManagement";
import { useAppleBilling } from "@/billing/useAppleBilling";
import { Button } from "@/components/ui/button";
import { useBillingState } from "@/state/useLocalState";
import { isIOS } from "@/utils/platform";
import { SettingsPage, SettingsSection } from "./SettingsPage";
import { BillingSubscriptions } from "./BillingSubscriptions";
import { ApplePurchaseRecovery } from "./ApplePurchaseRecovery";

export function BillingSettings() {
  const { billingStatus } = useBillingState();
  const apple = useAppleBilling();
  const accountId = useOpenSecret().auth.user?.user.id ?? "signed-out";

  const productName = billingStatus?.product_name ?? "";
  const normalizedProductName = productName.toLowerCase();
  const showUpgrade =
    !normalizedProductName.includes("max") && !normalizedProductName.includes("team");

  const periodLabel =
    billingStatus?.payment_provider === "subscription_pass" ||
    billingStatus?.payment_provider === "zaprite"
      ? "Expires"
      : "Current period ends";
  const endDate = billingPeriodDate(billingStatus?.current_period_end ?? null);

  return (
    <SettingsPage title="Billing" description="Review your plan and manage subscription access.">
      <SettingsSection title="Current plan">
        <div className="flex flex-col gap-5 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <div className="flex items-center gap-2">
              <CreditCard className="h-5 w-5 text-muted-foreground" />
              <p className="text-lg font-semibold">
                {billingStatus ? `${productName || "Current"} Plan` : "Loading plan..."}
              </p>
            </div>
            {endDate && (
              <p className="mt-1.5 text-sm text-muted-foreground">
                {periodLabel} on {endDate}
              </p>
            )}
          </div>
          <div className="flex flex-col gap-2 sm:flex-row">
            {showUpgrade && (
              <Button asChild variant="primary">
                <Link to="/pricing">
                  <Sparkles className="mr-2 h-4 w-4" />
                  Upgrade plan
                </Link>
              </Button>
            )}
          </div>
        </div>
      </SettingsSection>

      <BillingSubscriptions key={`subscriptions:${accountId}`} status={billingStatus} />
      <ApplePurchaseRecovery key={`recovery:${accountId}:${apple.ownerKey}`} apple={apple} />

      <SettingsSection
        title="API credits"
        description={
          isIOS()
            ? "View your extra credit balance and manage API access."
            : "View your extra credit balance or purchase credits for API and extended plan usage."
        }
      >
        <Button asChild variant="outline">
          <Link to="/settings/api" replace>
            <KeyRound className="mr-2 h-4 w-4" />
            Manage API and credits
          </Link>
        </Button>
      </SettingsSection>
    </SettingsPage>
  );
}
