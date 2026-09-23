import { useEffect, useState } from "react";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { AppEntryPage } from "@/components/AppEntryPage";
import { GuestPaymentWarningDialog } from "@/components/GuestPaymentWarningDialog";
import { PromoDialog, hasSeenPromo, markPromoAsSeen } from "@/components/PromoDialog";
import { useOpenSecret } from "@mapleai/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { getBillingService } from "@/billing/billingService";
import { useBillingState } from "@/state/useLocalState";
import type { DiscountResponse } from "@/billing/billingApi";
import { appUrl } from "@/config/domains";
import { useRouteMeta } from "@/utils/routeMeta";
import { getSafeInternalRedirect } from "@/utils/internalRedirect";
import { isIOS } from "@/utils/platform";

const appHomeUrl = appUrl("/");

type IndexSearchOptions = {
  login?: string;
  next?: string;
  team_setup?: boolean;
  credits_success?: boolean;
  api_settings?: boolean;
};

function validateSearch(search: Record<string, unknown>): IndexSearchOptions {
  return {
    login: search?.login === "true" ? "true" : undefined,
    next: getSafeInternalRedirect(search.next),
    team_setup: search?.team_setup === true || search?.team_setup === "true" ? true : undefined,
    credits_success:
      search?.credits_success === true || search?.credits_success === "true" ? true : undefined,
    api_settings:
      search?.api_settings === true || search?.api_settings === "true" ? true : undefined
  };
}

export const Route = createFileRoute("/")({
  component: Index,
  validateSearch
});

function Index() {
  const navigate = useNavigate();
  const os = useOpenSecret();
  const queryClient = useQueryClient();
  const showWebPromotions = !isIOS();
  const { setBillingStatus, billingStatus } = useBillingState();

  useRouteMeta({
    title: os.auth.user ? "Maple Research" : "Maple Research | Private AI Workspace",
    description: os.auth.user
      ? "Maple Research private AI workspace."
      : "Open Maple Research to sign up or log in to your private AI workspace.",
    canonicalUrl: appHomeUrl
  });

  const { login, next, team_setup, credits_success, api_settings } = Route.useSearch();

  const [showGuestPaymentWarning, setShowGuestPaymentWarning] = useState(false);
  const [promoDialogOpen, setPromoDialogOpen] = useState(false);

  // Proactively fetch billing status for authenticated users
  useQuery({
    queryKey: ["billingStatus"],
    queryFn: async () => {
      const billingService = getBillingService();
      const status = await billingService.getBillingStatus();
      setBillingStatus(status);
      return status;
    },
    enabled: !!os.auth.user
  });

  // Handle login redirect
  useEffect(() => {
    if (login === "true") {
      navigate({
        to: "/login",
        search: next ? { next } : undefined
      });
    }
  }, [login, next, navigate]);

  // Fetch active discount/promotion for promo dialog
  const { data: discount } = useQuery<DiscountResponse>({
    queryKey: ["discount"],
    queryFn: async () => {
      const billingService = getBillingService();
      return await billingService.getDiscount();
    },
    staleTime: 5 * 60 * 1000,
    enabled: !!os.auth.user && showWebPromotions
  });

  // Preserve legacy Team setup URLs by routing them into the dedicated Team settings page.
  useEffect(() => {
    if (team_setup && os.auth.user) {
      navigate({ to: "/settings/team", replace: true });
    }
  }, [team_setup, os.auth.user, navigate]);

  // Preserve the existing credit callback while moving its success state into API settings.
  useEffect(() => {
    if (credits_success && os.auth.user) {
      queryClient.invalidateQueries({ queryKey: ["apiCreditBalance"] });
      navigate({
        to: "/settings/api",
        search: { credits_success: true },
        replace: true
      });
    }
  }, [credits_success, os.auth.user, navigate, queryClient]);

  // Preserve legacy links that opened API Management directly.
  useEffect(() => {
    if (api_settings && os.auth.user) {
      navigate({ to: "/settings/api", replace: true });
    }
  }, [api_settings, os.auth.user, navigate]);

  // Check if guest user needs to pay
  const isGuestUser = os.auth.user?.user.login_method?.toLowerCase() === "guest";
  const isOnFreePlan = billingStatus?.product_name?.toLowerCase().includes("free") ?? false;
  const shouldShowGuestPaymentWarning = isGuestUser && isOnFreePlan && billingStatus !== null;

  // Show guest payment warning if needed (but not on pricing page)
  useEffect(() => {
    const currentPath = window.location.pathname;
    if (shouldShowGuestPaymentWarning && currentPath !== "/pricing") {
      setShowGuestPaymentWarning(true);
    } else {
      setShowGuestPaymentWarning(false);
    }
  }, [shouldShowGuestPaymentWarning]);

  // Show promo dialog for free users with active discount (one-time per promo)
  // This has LOWEST priority - don't show if other important dialogs should be visible
  useEffect(() => {
    if (!showWebPromotions || !os.auth.user || !billingStatus || !discount?.active) return;

    // Check if higher-priority dialogs should be shown
    const needsEmailVerification =
      !import.meta.env.DEV && !isGuestUser && !os.auth.user.user.email_verified;

    // Don't show promo if higher-priority dialogs are active
    if (needsEmailVerification || shouldShowGuestPaymentWarning) return;

    if (isOnFreePlan && !hasSeenPromo(discount.name)) {
      // Mark as seen IMMEDIATELY (before showing) to prevent bugs
      markPromoAsSeen(discount.name);
      setPromoDialogOpen(true);
    }
  }, [
    os.auth.user,
    billingStatus,
    discount,
    isGuestUser,
    isOnFreePlan,
    shouldShowGuestPaymentWarning,
    showWebPromotions
  ]);

  // Signed-out apex is now an app entry point; rich marketing lives on www.trymaple.ai.
  if (!os.auth.user) {
    return <AppEntryPage />;
  }

  // The authenticated home surface is mounted by the root route so it can remain alive while
  // the dedicated settings routes visually replace it.
  return (
    <>
      {/* Modals */}
      <GuestPaymentWarningDialog
        open={showGuestPaymentWarning}
        onOpenChange={setShowGuestPaymentWarning}
      />

      {/* Promo Dialog - shows once per promo for free users */}
      {showWebPromotions && discount?.active && (
        <PromoDialog open={promoDialogOpen} onOpenChange={setPromoDialogOpen} discount={discount} />
      )}
    </>
  );
}
