import { useOpenSecret } from "@mapleai/sdk";
import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { getBillingService } from "@/billing/billingService";
import { useAppleBilling } from "@/billing/useAppleBilling";
import { useAppleProducts } from "@/billing/useAppleProducts";
import {
  appleProductIds,
  appleSizes,
  type ApplePlan,
  type AppleSizeOffer
} from "@/billing/applePricingPolicy";
import { storeKit } from "@/services/storeKitService";
import { ApplePricing } from "./ApplePricing";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useEffect, useRef } from "react";
import { assertChatAccountCredential } from "@/services/chatAccountCredential";
import { FullPageMain } from "@/components/FullPageMain";

export function ApplePricingPage() {
  const os = useOpenSecret();
  const user = os.auth.user?.user;
  const apple = useAppleBilling();
  // Remount feedback and status requests on every authenticated credential owner.
  return (
    <AccountPricing
      key={`${user?.id ?? "signed-out"}:${apple.ownerKey ?? "no-session"}`}
      userId={user?.id ?? null}
      isGuest={user?.login_method?.toLowerCase() === "guest"}
    />
  );
}

function AccountPricing({ userId, isGuest }: { userId: string | null; isGuest: boolean }) {
  const active = useRef(true);
  const lifetime = useRef(0);
  useEffect(() => {
    active.current = true;
    const generation = lifetime.current + 1;
    lifetime.current = generation;
    return () => {
      active.current = false;
      lifetime.current = generation + 1;
    };
  }, []);
  const apple = useAppleBilling();
  const navigate = useNavigate();
  // Billing's product list names the App Store product of every plan it maps.
  // Without it the paywall still sells Pro and Max from their fixed identifiers.
  const billingProducts = useQuery({
    queryKey: ["billingProducts", "applePaywall"],
    queryFn: () => getBillingService().getProducts(),
    retry: 1,
    staleTime: 5 * 60 * 1000
  });
  const variant = import.meta.env.VITE_MAPLE_APP_VARIANT;
  const listed = appleSizes(billingProducts.data, appleProductIds(variant));
  const catalog = useAppleProducts(
    variant,
    undefined,
    undefined,
    Object.values(listed).flatMap((sizes) => sizes.map((size) => size.productId))
  );
  const sizes: Partial<Record<ApplePlan, AppleSizeOffer[]>> = {};
  for (const plan of Object.keys(listed) as ApplePlan[]) {
    const offers = (listed[plan] ?? []).flatMap((size) => {
      const product = catalog.sizes[size.productId];
      return product ? [{ ...size, product }] : [];
    });
    if (offers.length) sizes[plan] = offers;
  }
  const status = useQuery({
    queryKey: ["billingStatus", userId],
    queryFn: async () => {
      const generation = lifetime.current;
      if (!active.current) throw new Error("apple_billing_session_changed");
      assertChatAccountCredential(userId ?? undefined);
      const result = await getBillingService().getBillingStatus();
      if (!active.current || generation !== lifetime.current)
        throw new Error("apple_billing_session_changed");
      assertChatAccountCredential(userId ?? undefined);
      return result;
    },
    enabled: !!userId && apple.ready,
    retry: 1
  });

  return (
    // Give the native paywall its own bounded scroll area. Its content and
    // footer must retain their full height instead of shrinking into the viewport.
    <FullPageMain className="h-dvh min-h-0 overflow-y-auto bg-background px-4 py-10 text-foreground sm:px-6 [&>div]:shrink-0">
      <ApplePricing
        userId={userId}
        isGuest={isGuest}
        status={status.data}
        statusLoading={!!userId && (status.isPending || !apple.ready)}
        statusError={status.isError}
        ready={apple.ready}
        busy={apple.busy}
        recoveryError={apple.recoveryError}
        products={catalog.products}
        sizes={sizes}
        productsLoading={catalog.loading}
        productsError={catalog.error}
        purchase={apple.purchase}
        restore={apple.restore}
        retry={apple.retry}
        reloadPrices={catalog.retry}
        refreshStatus={() => {
          void status.refetch();
        }}
        manageApple={storeKit.manageSubscriptions}
        openEula={() =>
          openUrl("https://www.apple.com/legal/internet-services/itunes/dev/stdeula/")
        }
        navigate={(destination) => {
          void navigate({ to: destination });
        }}
      />
    </FullPageMain>
  );
}
