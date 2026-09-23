import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState, useEffect, useCallback, type ReactNode } from "react";
import { useOpenSecret } from "@mapleai/sdk";
import { FullPageMain } from "@/components/FullPageMain";
import { getBillingService } from "@/billing/billingService";
import { useQuery } from "@tanstack/react-query";
import {
  Loader2,
  Check,
  AlertTriangle,
  AlertCircle,
  Bitcoin,
  Tag,
  ChevronDown,
  ArrowLeft
} from "lucide-react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import type { DiscountResponse } from "@/billing/billingApi";
import { Badge } from "@/components/ui/badge";
import { useBillingState } from "@/state/useLocalState";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { PRICING_PLANS } from "@/config/pricingConfig";
import { VerificationModal } from "@/components/VerificationModal";
import { TeamSeatDialog } from "@/components/TeamSeatDialog";
import { ZapriteUpgradeDialog } from "@/components/ZapriteUpgradeDialog";
import { zapritePaidPlanButtonText, zapriteUpgradeTarget } from "@/billing/zapriteUpgrade";
import { isIOS, isAndroid, isMobile, isTauri } from "@/utils/platform";
import { cn } from "@/utils/utils";
import packageJson from "../../package.json";
import { ApplePricingPage } from "@/components/billing/ApplePricingPage";
import {
  hasAppleSubscriptionToManage,
  hasConfirmedBillingStatus
} from "@/billing/applePricingPolicy";
import { openAppleSubscriptionManagement } from "@/billing/appleSubscriptionManagement";
import { assertChatAccountCredential } from "@/services/chatAccountCredential";

// File type constants for upload features
const SUPPORTED_IMAGE_FORMATS = [".jpg", ".png", ".webp"];
const SUPPORTED_DOCUMENT_FORMATS = [".pdf", ".doc", ".docx", ".txt", ".md"];
const pricingPageClassName =
  "min-h-dvh bg-[#e2e2e2] py-12 pt-10 text-[#221a18] dark:bg-background dark:text-foreground sm:pt-14";

type PricingSearchParams = {
  selected_plan?: string;
  success?: boolean;
  canceled?: boolean;
};

interface Product {
  id: string;
  name: string;
  default_price: {
    unit_amount: number;
  };
  description?: string;
  active?: boolean;
  is_available?: boolean;
}

function PricingSkeletonCard() {
  return (
    <div className="relative flex min-h-[28rem] flex-col overflow-hidden rounded-lg border border-neutral-900/10 bg-white/70 p-5 text-[#221a18] shadow-sm backdrop-blur dark:border-white/10 dark:bg-neutral-900/70 dark:text-foreground sm:p-6">
      <div className="absolute inset-0 -translate-x-full animate-[shimmer_2s_infinite] bg-gradient-to-r from-transparent via-white/40 to-transparent dark:via-white/5"></div>
      <div className="flex h-full flex-col gap-5">
        <div className="h-6 w-1/2 rounded-md bg-neutral-900/10 dark:bg-white/10"></div>
        <div className="space-y-3">
          <div className="h-4 w-3/4 rounded-md bg-neutral-900/10 dark:bg-white/10"></div>
          <div className="h-4 w-5/6 rounded-md bg-neutral-900/10 dark:bg-white/10"></div>
          <div className="h-4 w-2/3 rounded-md bg-neutral-900/10 dark:bg-white/10"></div>
        </div>
        <div className="mt-auto h-8 w-1/3 rounded-md bg-neutral-900/10 dark:bg-white/10"></div>
        <div className="h-12 w-full rounded-md bg-neutral-900/10 dark:bg-white/10"></div>
      </div>
    </div>
  );
}

function PricingHero() {
  return (
    <section className="mx-auto flex w-full max-w-4xl flex-col items-center gap-5 px-4 pt-4 text-center sm:px-6 lg:px-8">
      <img src="/maple-research-icon.svg" alt="" className="h-14 w-14 rounded-[12px] shadow-sm" />
      <div className="space-y-4">
        <h1 className="font-displayWide text-4xl font-normal leading-tight text-[#221a18] dark:text-foreground">
          Simple, <span className="brand-gradient-text">Transparent</span> Pricing
        </h1>
        <div className="mx-auto max-w-2xl space-y-1 text-base leading-7 text-[#747474] dark:text-muted-foreground">
          <p>Start with our free tier and upgrade as you grow.</p>
          <p>All plans include end-to-end encrypted AI chat.</p>
        </div>
      </div>
    </section>
  );
}

function PricingReturnButton({
  isLoggedIn,
  onClick
}: {
  isLoggedIn: boolean;
  onClick: () => void;
}) {
  return (
    <div className="mx-auto flex w-full max-w-7xl px-4 sm:px-6 lg:px-8">
      <Button
        type="button"
        variant="ghost"
        onClick={onClick}
        className="h-9 gap-2 rounded-md bg-white/45 px-3 text-sm font-semibold text-[#747474] shadow-sm transition hover:bg-white/70 hover:text-[#221a18] dark:bg-white/[0.04] dark:text-muted-foreground dark:hover:bg-white/[0.08] dark:hover:text-foreground"
      >
        <ArrowLeft className="h-4 w-4" />
        {isLoggedIn ? "Chat" : "Home"}
      </Button>
    </div>
  );
}

function FAQItem({ question, children }: { question: string; children: ReactNode }) {
  return (
    <details className="group rounded-lg border border-neutral-900/10 bg-white/55 p-4 shadow-sm transition-colors open:bg-white/75 dark:border-white/10 dark:bg-neutral-900/55 dark:open:bg-neutral-900/75">
      <summary className="flex cursor-pointer list-none items-center justify-between gap-4 text-base font-semibold text-[#221a18] transition-colors hover:text-[hsl(var(--maple-primary-strong))] dark:text-foreground dark:hover:text-[hsl(var(--maple-primary))] [&::-webkit-details-marker]:hidden">
        <span>{question}</span>
        <ChevronDown className="h-4 w-4 shrink-0 text-[#747474] transition-transform group-open:rotate-180 dark:text-muted-foreground" />
      </summary>
      <div className="mt-4 text-sm leading-6 text-[#747474] dark:text-muted-foreground">
        {children}
      </div>
    </details>
  );
}

function PricingFAQ() {
  return (
    <section className="mx-auto mt-8 flex w-full max-w-5xl flex-col gap-6 rounded-lg border border-neutral-900/10 bg-white/65 p-5 text-[#221a18] shadow-sm backdrop-blur dark:border-white/10 dark:bg-neutral-900/65 dark:text-foreground sm:p-6">
      <h2 className="font-display text-3xl font-normal">FAQ</h2>

      <div className="flex flex-col gap-4">
        <FAQItem question="What is the difference between the plans?">
          <div className="space-y-2">
            <p>The plans are sized to grow with your needs.</p>
            <ul className="list-disc list-inside space-y-1 ml-4">
              <li>
                Free: 25 messages per week, resets Sunday 00:00 UTC. Max length on individual
                messages.
              </li>
              <li>Pro: Generous usage for power users with a high monthly cap</li>
              <li>Max: 200,000 monthly credits (10x Pro) for maximum power users</li>
              <li>Team: Even more usage per team member with unified billing</li>
              <li>Enterprise: Message us at support@trymaple.ai</li>
            </ul>
          </div>
        </FAQItem>

        <FAQItem question="How private is Maple?">
          <p>
            Encrypted end to end. Maple uses confidential computing to secure the code that access
            user data and LLM data. Your account has its own private key that encrypts your chats
            and the responses from the AI model. Every user has their own personal data vault that
            can't be read by anyone else, not even us.
          </p>
        </FAQItem>

        <FAQItem question="How do you synchronize my chat history across devices?">
          <p>
            We use a secure synchronization protocol that ensures your encrypted chat history is
            synced across all your devices. This means that you can start a conversation on one
            device and pick it up where you left off on another device, without compromising your
            security or privacy.
          </p>
        </FAQItem>

        <FAQItem question="Is this safe to use with my company's confidential information?">
          <p>
            The service is encrypted end-to-end, so your confidential information is private between
            you and the AI. Consult your company's security policy.
          </p>
        </FAQItem>

        <FAQItem question="Can companies use my data to train their AI models?">
          <p>
            No. When you chat with AI in Maple, nobody knows what is being said back and forth. Thus
            data is not able to be used for training new AI models by any company.
          </p>
        </FAQItem>

        <FAQItem question="Which file types are supported for document and image upload?">
          <div className="space-y-4">
            <p>We support a range of file types for both images and documents.</p>
            <div>
              <strong>Images:</strong>
              <ul className="list-disc list-inside ml-4 mt-2 space-y-1">
                {SUPPORTED_IMAGE_FORMATS.map((format) => (
                  <li key={format}>
                    <code className="bg-foreground/10 px-1 py-0.5 rounded text-sm">{format}</code>
                  </li>
                ))}
              </ul>
            </div>
            <div>
              <strong>Documents (Desktop/Mobile apps only):</strong>
              <ul className="list-disc list-inside ml-4 mt-2 space-y-1">
                {SUPPORTED_DOCUMENT_FORMATS.map((format) => (
                  <li key={format}>
                    <code className="bg-foreground/10 px-1 py-0.5 rounded text-sm">{format}</code>
                  </li>
                ))}
              </ul>
            </div>
            <p>There is a 1 file limit per chat prompt with a 10MB file size limit per file.</p>
          </div>
        </FAQItem>

        <FAQItem question="Can I use my subscription for API access?">
          <div className="space-y-2">
            <p>
              Yes! Pro, Max, and Team plans include API access. Your subscription credits work
              seamlessly with the API.
            </p>
            <ul className="list-disc list-inside space-y-1 ml-4">
              <li>Use your plan credits via the API</li>
              <li>When plan credits run out, extra credits kick in automatically</li>
              <li>Purchase extra credits to extend your usage anytime</li>
            </ul>
          </div>
        </FAQItem>

        <FAQItem question="How did you build this?">
          <p>
            Maple is made using{" "}
            <a
              href="https://opensecret.cloud"
              className="text-[hsl(var(--maple-primary-strong))] underline underline-offset-4 hover:text-[hsl(var(--maple-primary))] dark:text-[hsl(var(--maple-primary))]"
            >
              OpenSecret
            </a>
            , an encrypted backend for developers. It handles private keys, encrypted data sync,
            private AI, and more automatically. We're building OpenSecret as a way to bring better
            privacy to users by turning on encryption by default. If you're a developer, go see how
            easy it is to build with.
          </p>
        </FAQItem>
      </div>
    </section>
  );
}

function PricingPage() {
  // Keep the legacy checkout effects unmounted on iOS, including selected_plan
  // callbacks. IAP availability never falls back to Stripe or Zaprite checkout.
  return isIOS() ? <ApplePricingPage /> : <LegacyPricingPage />;
}

function LegacyPricingPage() {
  const [checkoutError, setCheckoutError] = useState<string>("");
  const [portalError, setPortalError] = useState<string | null>(null);
  const [loadingProductId, setLoadingProductId] = useState<string | null>(null);
  const [useBitcoin, setUseBitcoin] = useState(false);
  const [showTeamSeatDialog, setShowTeamSeatDialog] = useState(false);
  const [pendingTeamProductId, setPendingTeamProductId] = useState<string | null>(null);
  const [zapriteUpgradeProductId, setZapriteUpgradeProductId] = useState<string | null>(null);
  const [zapriteUpgradeOpen, setZapriteUpgradeOpen] = useState(false);
  const navigate = useNavigate();
  const os = useOpenSecret();
  const { setBillingStatus } = useBillingState();
  const isLoggedIn = !!os.auth.user;
  const billingUserId = os.auth.user?.user.id;
  const isGuestUser = os.auth.user?.user.login_method?.toLowerCase() === "guest";
  const { selected_plan } = Route.useSearch();

  // Use platform detection functions
  const isIOSPlatform = isIOS();
  const isAndroidPlatform = isAndroid();
  const isMobilePlatform = isMobile();

  // Fetch billing status if user is logged in
  const {
    data: freshBillingStatus,
    isLoading: isBillingStatusLoading,
    isError: isBillingStatusError,
    refetch: refetchBillingStatus
  } = useQuery({
    queryKey: ["billingStatus", billingUserId],
    queryFn: async () => {
      const billingService = getBillingService();
      assertChatAccountCredential(billingUserId);
      const status = await billingService.getBillingStatus();
      assertChatAccountCredential(billingUserId);
      setBillingStatus(status);
      return status;
    },
    enabled: isLoggedIn
  });
  const billingStatusConfirmed = hasConfirmedBillingStatus(
    freshBillingStatus,
    isBillingStatusLoading,
    isBillingStatusError
  );

  // Auto-enable Bitcoin toggle for Zaprite users (except on mobile platforms) and guest users
  useEffect(() => {
    if ((freshBillingStatus?.payment_provider === "zaprite" && !isMobilePlatform) || isGuestUser) {
      setUseBitcoin(true);
    }
  }, [freshBillingStatus?.payment_provider, isMobilePlatform, isGuestUser]);

  // Always try to fetch portal URL if logged in
  const { data: portalUrl } = useQuery({
    queryKey: ["portalUrl", billingUserId],
    queryFn: async () => {
      if (!isLoggedIn) return null;
      const billingService = getBillingService();
      try {
        assertChatAccountCredential(billingUserId);
        const url = await billingService.getPortalUrl();
        assertChatAccountCredential(billingUserId);
        return url;
      } catch (error) {
        console.error("Error fetching portal URL:", error);
        return null;
      }
    },
    enabled: isLoggedIn
  });

  // Fetch active discount/promotion
  const { data: discount } = useQuery<DiscountResponse>({
    queryKey: ["discount"],
    queryFn: async () => {
      const billingService = getBillingService();
      return await billingService.getDiscount();
    },
    staleTime: 5 * 60 * 1000 // Cache for 5 minutes
  });

  const {
    data: products,
    error: productsError,
    isLoading: productsLoading
  } = useQuery({
    queryKey: ["products", isIOSPlatform, isAndroidPlatform],
    queryFn: async () => {
      try {
        const billingService = getBillingService();
        // Send version for mobile builds (iOS needs it for App Store restrictions)
        if (isIOSPlatform || isAndroidPlatform) {
          // Get version from package.json
          const version = `v${packageJson.version}`;
          console.log("[Billing] Fetching products with version:", version);
          return await billingService.getProducts(version);
        }
        return await billingService.getProducts();
      } catch (error) {
        console.error("Error fetching products:", error);
        throw error;
      }
    },
    retry: 1 // Only retry once for faster error feedback
  });

  // Handle payment callback status from URL params
  const { success, canceled } = Route.useSearch();

  // Check if team plan purchase and redirect after success
  useEffect(() => {
    if (success && freshBillingStatus?.product_name?.toLowerCase().includes("team")) {
      // Redirect to home with team_setup param
      navigate({ to: "/", search: { team_setup: true }, replace: true });
    }
  }, [success, freshBillingStatus, navigate]);

  const getButtonText = (product: Product) => {
    if (loadingProductId === product.id) {
      return (
        <>
          <Loader2 className="w-5 h-5 animate-spin" />
          Processing...
        </>
      );
    }

    const targetPlanName = product.name.toLowerCase();
    const isTeamPlan = targetPlanName.includes("team");
    const isFreeplan = targetPlanName.includes("free");

    // Show "Not available in app" for iOS paid plans if server says not available
    // Android can support paid plans (no App Store restrictions)
    if (isIOSPlatform && !isFreeplan && product.is_available === false) {
      return "Not available in app";
    }

    // Show Start Chatting for all plans when not logged in
    if (!isLoggedIn) {
      return "Start Chatting";
    }

    if (!billingStatusConfirmed) return isFreeplan ? "Start Chatting" : "Reload current plan";

    if (hasAppleSubscriptionToManage(freshBillingStatus)) return "Manage with Apple";

    const currentPlanName = freshBillingStatus?.product_name?.toLowerCase();
    const isCurrentPlan = currentPlanName === targetPlanName;

    const zapriteButton = zapritePaidPlanButtonText(freshBillingStatus, product, isCurrentPlan);
    if (zapriteButton) {
      return zapriteButton;
    }

    // If user is on subscription pass, only show button for current plan
    if (freshBillingStatus?.payment_provider === "subscription_pass") {
      if (isCurrentPlan) {
        return "Start Chatting";
      }
      return "Contact Us";
    }

    // For team plan
    if (isTeamPlan) {
      if (isCurrentPlan) {
        return "Manage Plan";
      }
      return "Upgrade";
    }

    // For free plan
    if (targetPlanName.includes("free")) {
      if (isCurrentPlan) {
        return "Start Chatting";
      }
      return "Downgrade";
    }

    // For paid plans
    if (isCurrentPlan) {
      return "Manage Plan";
    }

    // Only compare plan levels if we have both products and current plan name
    if (currentPlanName && products) {
      const sortedProducts = [...products].sort(
        (a, b) => a.default_price.unit_amount - b.default_price.unit_amount
      );
      const currentPlanIndex = sortedProducts.findIndex(
        (p) => p.name.toLowerCase() === currentPlanName
      );
      const targetPlanIndex = sortedProducts.findIndex(
        (p) => p.name.toLowerCase() === targetPlanName
      );

      if (currentPlanIndex !== -1 && targetPlanIndex !== -1 && targetPlanIndex > currentPlanIndex) {
        return "Upgrade";
      }
    }

    return "Downgrade";
  };

  const newHandleSubscribe = useCallback(
    async (productId: string, quantity?: number) => {
      if (!isLoggedIn) {
        navigate({ to: "/signup" });
        return;
      }

      if (!billingStatusConfirmed) {
        setPortalError(
          "Your current plan could not be checked. Reload it before starting a subscription."
        );
        return;
      }

      // Apple subscriptions are managed by Apple on every platform. Never
      // create a second paid subscription through the legacy checkout path.
      if (hasAppleSubscriptionToManage(freshBillingStatus)) {
        try {
          await openAppleSubscriptionManagement();
        } catch {
          setPortalError("Unable to open Apple subscription management. Please try again.");
        }
        return;
      }

      // Check if email is verified before proceeding (skip for guests and in dev mode)
      if (!import.meta.env.DEV && !isGuestUser && !os.auth.user?.user.email_verified) {
        console.log("Email verification required before checkout");
        return;
      }

      setLoadingProductId(productId);
      try {
        const billingService = getBillingService();

        // For guest users, pass empty string (backend infers user from JWT token)
        const email = isGuestUser ? "" : os.auth.user?.user.email || "";

        if (!email && !isGuestUser) {
          throw new Error("User email not found");
        }

        // For mobile platforms (iOS and Android), use Universal Links / App Links
        if (isMobilePlatform) {
          if (useBitcoin) {
            await billingService.createZapriteCheckoutSession(
              email,
              productId,
              `https://trymaple.ai/payment-success?source=zaprite`,
              quantity
            );
          } else {
            await billingService.createCheckoutSession(
              email,
              productId,
              `https://trymaple.ai/payment-success?source=stripe`,
              `https://trymaple.ai/payment-canceled?source=stripe`,
              quantity
            );
          }
        } else if (isTauri()) {
          // For Tauri desktop, use trymaple.ai since tauri://localhost won't work in external browser
          if (useBitcoin) {
            await billingService.createZapriteCheckoutSession(
              email,
              productId,
              `https://trymaple.ai/pricing?success=true`,
              quantity
            );
          } else {
            await billingService.createCheckoutSession(
              email,
              productId,
              `https://trymaple.ai/pricing?success=true`,
              `https://trymaple.ai/pricing?canceled=true`,
              quantity
            );
          }
        } else {
          // For web, use regular URLs
          if (useBitcoin) {
            await billingService.createZapriteCheckoutSession(
              email,
              productId,
              `${window.location.origin}/pricing?success=true`,
              quantity
            );
          } else {
            await billingService.createCheckoutSession(
              email,
              productId,
              `${window.location.origin}/pricing?success=true`,
              `${window.location.origin}/pricing?canceled=true`,
              quantity
            );
          }
        }
      } catch (err) {
        console.error("Subscribe error:", err);
        setCheckoutError(err instanceof Error ? err.message : "Failed to start checkout");
      } finally {
        setLoadingProductId(null);
      }
    },
    [
      isLoggedIn,
      navigate,
      os.auth.user?.user.email,
      os.auth.user?.user.email_verified,
      isGuestUser,
      useBitcoin,
      isMobilePlatform,
      freshBillingStatus,
      billingStatusConfirmed
    ]
  );

  const handleButtonClick = useCallback(
    (product: Product) => {
      setPortalError(null);
      const targetPlanName = product.name.toLowerCase();
      const isFreeplan = targetPlanName.includes("free");
      const isTeamPlan = targetPlanName.includes("team");

      // Disable clicks for iOS paid plans if server says not available
      // Android can support paid plans
      if (isIOSPlatform && !isFreeplan && product.is_available === false) {
        return;
      }

      if (!isLoggedIn) {
        if (!targetPlanName.includes("free")) {
          // For paid plans, redirect to signup with the plan selection
          navigate({
            to: "/signup",
            search: {
              next: "/pricing",
              selected_plan: product.id
            }
          });
          return;
        }
        navigate({ to: "/signup" });
        return;
      }

      if (!billingStatusConfirmed) {
        if (isFreeplan) navigate({ to: "/" });
        else void refetchBillingStatus();
        return;
      }

      if (hasAppleSubscriptionToManage(freshBillingStatus)) {
        void openAppleSubscriptionManagement().catch(() => {
          setPortalError("Unable to open Apple subscription management. Please try again.");
        });
        return;
      }

      if (freshBillingStatus?.payment_provider === "zaprite") {
        const currentPlanName = freshBillingStatus.product_name?.toLowerCase();
        if (currentPlanName === targetPlanName) {
          navigate({ to: "/" });
          return;
        }
        if (zapriteUpgradeTarget(freshBillingStatus, product) && !isIOSPlatform) {
          setZapriteUpgradeProductId(product.id);
          setZapriteUpgradeOpen(true);
          return;
        }
        if (isTauri()) {
          import("@tauri-apps/api/core")
            .then((coreModule) => {
              return coreModule.invoke("plugin:opener|open_url", {
                url: "mailto:support@trymaple.ai"
              });
            })
            .catch(() => {
              window.location.href = "mailto:support@trymaple.ai";
            });
        } else {
          window.location.href = "mailto:support@trymaple.ai";
        }
        return;
      }

      const currentPlanName = freshBillingStatus?.product_name?.toLowerCase();
      const isCurrentlyOnFreePlan = currentPlanName?.includes("free");
      const isTargetFreePlan = targetPlanName.includes("free");
      const isCurrentPlan = currentPlanName === targetPlanName;

      // If user is on subscription pass
      if (freshBillingStatus?.payment_provider === "subscription_pass") {
        if (isCurrentPlan) {
          // Current plan: go home
          navigate({ to: "/" });
        } else {
          // Other plans: contact support
          // Use Tauri opener plugin for all Tauri platforms, fallback to window.location for web
          if (isTauri()) {
            import("@tauri-apps/api/core")
              .then((coreModule) => {
                return coreModule.invoke("plugin:opener|open_url", {
                  url: "mailto:support@trymaple.ai"
                });
              })
              .then(() => {
                console.log("[Contact] Successfully opened mailto link with Tauri opener");
              })
              .catch((err) => {
                console.error("[Contact] Failed to open mailto link with Tauri opener:", err);
                // Fallback for web or if Tauri fails
                window.location.href = "mailto:support@trymaple.ai";
              });
          } else {
            window.location.href = "mailto:support@trymaple.ai";
          }
        }
        return;
      }

      // If on free plan and clicking free plan, go home
      if (isCurrentlyOnFreePlan && isTargetFreePlan) {
        navigate({ to: "/" });
        return;
      }

      // If user is on free plan and clicking a paid plan, use checkout URL
      if (isCurrentlyOnFreePlan && !isTargetFreePlan) {
        // For team plans, show seat selection dialog first
        if (isTeamPlan) {
          setPendingTeamProductId(product.id);
          setShowTeamSeatDialog(true);
          return;
        }
        newHandleSubscribe(product.id);
        return;
      }

      // For all other cases (upgrades/downgrades between paid plans, or downgrades to free),
      // use portal URL if it exists
      if (portalUrl) {
        // Open in external browser for all Tauri platforms (mobile and desktop)
        if (isTauri()) {
          console.log(
            "[Billing] Tauri platform detected, using opener plugin to launch external browser for portal"
          );

          // Use the Tauri opener plugin for all Tauri platforms
          import("@tauri-apps/api/core")
            .then((coreModule) => {
              return coreModule.invoke("plugin:opener|open_url", { url: portalUrl });
            })
            .then(() => {
              console.log("[Billing] Successfully opened portal URL in external browser");
            })
            .catch((err) => {
              console.error("[Billing] Failed to open external browser:", err);
              if (isMobilePlatform) {
                alert("Failed to open browser. Please try again.");
              } else {
                // Fallback to window.open on desktop
                window.open(portalUrl, "_blank", "noopener");
              }
            });
        } else {
          // Default browser opening for web platforms
          window.open(portalUrl, "_blank", "noopener");
        }
        return;
      }

      // If the user is already on a paid plan (including team) and portal URL failed to load,
      // show an error instead of silently falling through to checkout
      if (isCurrentPlan) {
        setPortalError(
          "Unable to open subscription management. Please try again or contact support@trymaple.ai."
        );
        return;
      }

      // If no portal URL exists and it's not a free plan user upgrading,
      // create checkout session
      // For team plans, show seat selection dialog first
      if (isTeamPlan) {
        setPendingTeamProductId(product.id);
        setShowTeamSeatDialog(true);
        return;
      }
      newHandleSubscribe(product.id);
    },
    [
      isLoggedIn,
      freshBillingStatus,
      navigate,
      portalUrl,
      newHandleSubscribe,
      isIOSPlatform,
      isMobilePlatform,
      billingStatusConfirmed,
      refetchBillingStatus
    ]
  );

  const handleTeamSeatConfirm = useCallback(
    (seats: number) => {
      if (pendingTeamProductId) {
        newHandleSubscribe(pendingTeamProductId, seats);
        setPendingTeamProductId(null);
      }
    },
    [pendingTeamProductId, newHandleSubscribe]
  );

  useEffect(() => {
    let isSubscribed = true;

    // A selected_plan callback from an earlier checkout must not launch a new
    // provider checkout or repeatedly open Apple's management page.
    if (hasAppleSubscriptionToManage(freshBillingStatus)) return;
    if (!billingStatusConfirmed) return;

    // If user is logged in and there's a selected plan, trigger checkout (except on iOS for paid plans)
    if (isLoggedIn && selected_plan && !isBillingStatusLoading) {
      if (loadingProductId) return; // Prevent multiple triggers
      const product = products?.find((p) => p.id === selected_plan);
      if (product && isSubscribed) {
        handleButtonClick(product);
      }
    }

    return () => {
      isSubscribed = false;
    };
  }, [
    isLoggedIn,
    selected_plan,
    isBillingStatusLoading,
    products,
    loadingProductId,
    handleButtonClick,
    freshBillingStatus,
    billingStatusConfirmed,
    isIOSPlatform
  ]);

  // Show loading state if we're fetching initial data
  if (productsLoading || (isLoggedIn && isBillingStatusLoading)) {
    return (
      <>
        <FullPageMain className={pricingPageClassName}>
          <PricingReturnButton isLoggedIn={isLoggedIn} onClick={() => navigate({ to: "/" })} />
          <PricingHero />

          <div className="mx-auto grid w-full max-w-7xl grid-cols-1 gap-5 px-4 pt-6 sm:px-6 md:grid-cols-4 lg:px-8">
            <PricingSkeletonCard />
            <PricingSkeletonCard />
            <PricingSkeletonCard />
            <PricingSkeletonCard />
          </div>

          <PricingFAQ />
        </FullPageMain>
      </>
    );
  }

  // Show error state for any errors (products error or checkout error)
  if (productsError || checkoutError) {
    const errorMessage =
      checkoutError ||
      (productsError instanceof Error ? productsError.message : "An unexpected error occurred");

    return (
      <>
        <FullPageMain className={pricingPageClassName}>
          <PricingReturnButton isLoggedIn={isLoggedIn} onClick={() => navigate({ to: "/" })} />
          <PricingHero />

          <div className="mx-auto grid w-full max-w-5xl grid-cols-1 px-4 pt-6 sm:px-6 lg:px-8">
            <div className="col-span-full flex flex-col rounded-lg border border-neutral-900/10 bg-white/70 p-8 text-[#221a18] shadow-sm backdrop-blur dark:border-white/10 dark:bg-neutral-900/70 dark:text-foreground">
              <div className="flex flex-col items-center text-center gap-4">
                <div className="rounded-full bg-maple-error/10 p-3">
                  <AlertTriangle className="w-6 h-6 text-maple-error" />
                </div>
                <div className="space-y-2">
                  <h3 className="text-xl font-medium">Unable to Load Pricing</h3>
                  <p className="text-[#747474] dark:text-muted-foreground">{errorMessage}</p>
                </div>
                <Button
                  variant="primary"
                  onClick={() => window.location.reload()}
                  className="mt-4 h-11 px-8 text-base"
                >
                  Retry
                </Button>
              </div>
            </div>
          </div>

          <PricingFAQ />
        </FullPageMain>
      </>
    );
  }

  return (
    <>
      <FullPageMain className={pricingPageClassName}>
        <PricingReturnButton isLoggedIn={isLoggedIn} onClick={() => navigate({ to: "/" })} />
        <PricingHero />

        {/* Payment Callback Status Messages */}
        {isLoggedIn && !billingStatusConfirmed && (
          <div
            role="alert"
            className="mx-auto mt-4 flex w-full max-w-7xl flex-wrap items-center justify-between gap-3 rounded-lg border border-maple-warning/35 bg-maple-warning/10 p-4 text-sm"
          >
            <p>Your current plan could not be checked. Reload it before starting a subscription.</p>
            <Button variant="outline" onClick={() => void refetchBillingStatus()}>
              Reload current plan
            </Button>
          </div>
        )}
        {success && freshBillingStatus?.payment_provider !== "zaprite" && (
          <div className="w-full max-w-7xl mx-auto mt-4 px-4 sm:px-6 lg:px-8">
            <div className="flex items-center gap-3 rounded-lg border border-maple-success/30 bg-maple-success/10 p-4 text-maple-success">
              <div className="rounded-full bg-maple-success/10 p-1">
                <Check className="w-5 h-5" />
              </div>
              <p>Payment successful! Your subscription has been updated.</p>
            </div>
          </div>
        )}

        {freshBillingStatus?.pending_plan_change && (
          <div className="w-full max-w-7xl mx-auto mt-4 px-4 sm:px-6 lg:px-8">
            <div className="flex items-center justify-between gap-3 rounded-lg border border-maple-warning/35 bg-maple-warning/10 p-4 text-maple-warning">
              <p>
                Upgrade to {freshBillingStatus.pending_plan_change.target_plan_name} is waiting for
                payment.
              </p>
              <Button
                variant="outline"
                onClick={() => {
                  setZapriteUpgradeProductId(null);
                  setZapriteUpgradeOpen(true);
                }}
              >
                Continue
              </Button>
            </div>
          </div>
        )}

        {canceled && (
          <div className="w-full max-w-7xl mx-auto mt-4 px-4 sm:px-6 lg:px-8">
            <div className="flex items-center gap-3 rounded-lg border border-maple-warning/35 bg-maple-warning/10 p-4 text-maple-warning">
              <div className="rounded-full bg-maple-warning/10 p-1">
                <AlertTriangle className="w-5 h-5" />
              </div>
              <p>Payment canceled. Your subscription remains unchanged.</p>
            </div>
          </div>
        )}

        {/* Portal Error Message */}
        {portalError && (
          <div className="w-full max-w-7xl mx-auto mt-4 px-4 sm:px-6 lg:px-8">
            <Alert variant="destructive">
              <AlertCircle className="h-4 w-4" />
              <AlertDescription>{portalError}</AlertDescription>
            </Alert>
          </div>
        )}

        {/* Promotion Banner */}
        {discount?.active && (
          <div className="w-full max-w-7xl mx-auto mt-4 px-4 sm:px-6 lg:px-8">
            <div className="flex items-center gap-3 rounded-lg border border-[hsl(var(--maple-primary))]/25 bg-gradient-to-r from-[hsl(var(--maple-primary))]/10 to-[hsl(var(--maple-primary-strong))]/10 p-4">
              <div className="rounded-full bg-gradient-to-b from-[hsl(var(--maple-primary))] to-[hsl(var(--maple-primary-strong))] p-2">
                <Tag className="w-5 h-5 text-[hsl(var(--maple-on-primary))]" />
              </div>
              <div className="flex-1">
                <p className="font-medium text-[#221a18] dark:text-foreground">{discount.name}</p>
                <p className="text-sm text-[#747474] dark:text-muted-foreground">
                  {discount.description}
                </p>
              </div>
              <Badge className="bg-gradient-to-b from-[hsl(var(--maple-primary))] to-[hsl(var(--maple-primary-strong))] text-[hsl(var(--maple-on-primary))] text-base px-3 py-1">
                {discount.percent_off}% OFF
              </Badge>
            </div>
          </div>
        )}

        {(() => {
          const filteredPlans = PRICING_PLANS;

          const gridColumns = filteredPlans.length === 4 ? "lg:grid-cols-4" : "lg:grid-cols-3";

          return (
            <div
              className={`mx-auto grid w-full max-w-7xl grid-cols-1 ${gridColumns} gap-5 px-4 pt-6 sm:px-6 lg:px-8`}
            >
              {filteredPlans.map((plan) => {
                // Find the matching product from server data
                const product = products?.find(
                  (p) => p.name.toLowerCase() === plan.name.toLowerCase() && p.active
                );

                // If no product found from server, use plan data
                if (!product && plan.name !== "Free") return null;

                const isCurrentPlan =
                  isLoggedIn &&
                  freshBillingStatus?.product_name?.toLowerCase() === plan.name.toLowerCase();
                const isTeamPlan = plan.name.toLowerCase().includes("team");
                const isFreeplan = plan.name.toLowerCase().includes("free");

                // Calculate prices
                let monthlyOriginalPrice = plan.price.replace("$", "");
                let monthlyPrice = monthlyOriginalPrice;

                if (product) {
                  monthlyOriginalPrice = (product.default_price.unit_amount / 100).toFixed(0);
                  monthlyPrice = monthlyOriginalPrice;
                }

                // Determine active discount percentage
                const promoDiscountPercent = discount?.active ? discount.percent_off : 0;
                const promoDurationMonths = discount?.active ? discount.duration_months : undefined;
                const bitcoinYearlyDiscountPercent = 10;

                // Use promo discount if active, otherwise fall back to Bitcoin yearly discount
                const effectiveDiscountPercent =
                  promoDiscountPercent > 0 ? promoDiscountPercent : bitcoinYearlyDiscountPercent;
                const effectiveMultiplier = (100 - effectiveDiscountPercent) / 100;

                // Calculate yearly prices for Bitcoin (with effective discount)
                const yearlyDiscountedPrice = product
                  ? (
                      Math.floor(product.default_price.unit_amount * 12 * effectiveMultiplier) / 100
                    ).toFixed(2)
                  : (Number(monthlyOriginalPrice) * 12 * effectiveMultiplier).toFixed(2);

                // Calculate monthly equivalent of yearly Bitcoin price
                const monthlyEquivalentPrice = (Number(yearlyDiscountedPrice) / 12).toFixed(2);

                // Calculate promo discounted price (applies to monthly)
                const promoMultiplier = (100 - promoDiscountPercent) / 100;
                const promoDiscountedPrice = (
                  Number(monthlyOriginalPrice) * promoMultiplier
                ).toFixed(0);

                // Determine display price based on active discounts
                let displayPrice = monthlyPrice;
                if (!isFreeplan) {
                  if (useBitcoin && !isTeamPlan) {
                    displayPrice = monthlyEquivalentPrice;
                  } else if (promoDiscountPercent > 0) {
                    displayPrice = promoDiscountedPrice;
                  }
                }

                // Determine which discount badge to show
                // Promo takes precedence over Bitcoin discount when active
                const hasActivePromo = promoDiscountPercent > 0;
                const showPromoBadge = !isFreeplan && hasActivePromo && !(useBitcoin && isTeamPlan);
                const showBitcoinBadge =
                  !isFreeplan && useBitcoin && !isTeamPlan && !hasActivePromo;

                return (
                  <div
                    key={plan.name}
                    className={cn(
                      "relative flex min-h-[31rem] flex-col overflow-hidden rounded-lg border bg-white/70 p-5 text-[#221a18] shadow-sm backdrop-blur transition-all duration-200 hover:border-[hsl(var(--maple-primary))]/40 hover:shadow-md dark:bg-neutral-900/70 dark:text-foreground sm:p-6",
                      plan.popular && !isCurrentPlan
                        ? "border-[hsl(var(--maple-primary))]/60 shadow-[0_14px_40px_rgba(var(--maple-primary-rgb),0.16)]"
                        : "border-neutral-900/10 dark:border-white/10",
                      isCurrentPlan && "ring-2 ring-[hsl(var(--maple-primary))]",
                      useBitcoin && isTeamPlan && "cursor-not-allowed opacity-55"
                    )}
                  >
                    {plan.popular && !isCurrentPlan && (
                      <div className="absolute inset-x-0 top-0 h-1 bg-gradient-to-r from-[hsl(var(--maple-primary))] to-[hsl(var(--maple-primary-strong))]" />
                    )}
                    <div className="flex h-full flex-col gap-5">
                      <div className="flex min-h-7 flex-wrap items-center gap-2">
                        {plan.popular && !isCurrentPlan && (
                          <Badge className="bg-gradient-to-b from-[hsl(var(--maple-primary))] to-[hsl(var(--maple-primary-strong))] text-[hsl(var(--maple-on-primary))]">
                            Most Popular
                          </Badge>
                        )}
                        {isCurrentPlan && (
                          <Badge className="bg-[hsl(var(--maple-tertiary-container))] text-[hsl(var(--maple-tertiary))]">
                            Current Plan
                          </Badge>
                        )}
                        {showPromoBadge && (
                          <Badge className="bg-[hsl(var(--maple-primary-container))] text-[hsl(var(--maple-primary-strong))] dark:bg-[hsl(var(--maple-primary))]/20 dark:text-[hsl(var(--maple-primary))]">
                            {promoDiscountPercent}% OFF
                          </Badge>
                        )}
                        {showBitcoinBadge && (
                          <Badge className="bg-[hsl(var(--maple-primary-container))] text-[hsl(var(--maple-primary-strong))] dark:bg-[hsl(var(--maple-primary))]/20 dark:text-[hsl(var(--maple-primary))]">
                            {bitcoinYearlyDiscountPercent}% OFF
                          </Badge>
                        )}
                      </div>

                      <div className="space-y-3">
                        <h3 className="flex items-center gap-2 font-display text-3xl font-normal leading-none">
                          {plan.name}
                          {useBitcoin && !isFreeplan && !isTeamPlan ? " (Yearly)" : ""}
                          {isCurrentPlan && <Check className="w-5 h-5 text-maple-success" />}
                        </h3>

                        <p className="text-sm leading-6 text-[#747474] dark:text-muted-foreground">
                          {isTeamPlan && useBitcoin
                            ? "Team plan is not available with Bitcoin payment."
                            : plan.description}
                        </p>
                      </div>

                      {/* Features List */}
                      <div className="flex flex-1 flex-col gap-2.5">
                        {plan.features.map((feature, index) => (
                          <div key={index} className="flex items-start gap-2 text-sm leading-5">
                            {feature.text !== "" &&
                              (feature.icon ||
                                (feature.included ? (
                                  <Check className="w-4 h-4 text-maple-success mt-0.5 flex-shrink-0" />
                                ) : null))}
                            <span
                              className={
                                feature.included
                                  ? "text-[#221a18]/80 dark:text-foreground/80"
                                  : "text-[#747474] dark:text-muted-foreground"
                              }
                            >
                              {feature.text}
                            </span>
                          </div>
                        ))}
                      </div>

                      <div className="flex flex-col">
                        {!isFreeplan ? (
                          <>
                            <div className="flex flex-wrap items-center gap-2">
                              <span className="text-3xl font-semibold">${displayPrice}</span>
                              {(showBitcoinBadge || showPromoBadge) && (
                                <span className="text-lg line-through text-[#747474] dark:text-muted-foreground">
                                  ${monthlyOriginalPrice}
                                </span>
                              )}
                              <div className="flex flex-col text-sm leading-5 text-[#747474] dark:text-muted-foreground">
                                <span>{isTeamPlan ? "per user" : ""}</span>
                                <span>per month</span>
                              </div>
                            </div>
                            <div className="mt-1 space-y-1">
                              {showBitcoinBadge && (
                                <>
                                  <p className="text-sm font-medium text-[#221a18]/90 dark:text-foreground/90">
                                    Billed yearly at ${yearlyDiscountedPrice}
                                  </p>
                                  <p className="text-xs text-[#747474] dark:text-muted-foreground">
                                    Save {bitcoinYearlyDiscountPercent}% with annual billing
                                  </p>
                                </>
                              )}
                              {showPromoBadge && useBitcoin && !isTeamPlan && (
                                <>
                                  <p className="text-sm font-medium text-[#221a18]/90 dark:text-foreground/90">
                                    Billed yearly at ${yearlyDiscountedPrice}
                                  </p>
                                  <p className="text-xs text-[#747474] dark:text-muted-foreground">
                                    Save {promoDiscountPercent}% with annual billing
                                  </p>
                                </>
                              )}
                              {showPromoBadge && !useBitcoin && (
                                <p className="text-xs text-[#747474] dark:text-muted-foreground">
                                  {promoDiscountPercent}% off
                                  {promoDurationMonths
                                    ? ` for first ${promoDurationMonths} month${promoDurationMonths > 1 ? "s" : ""}`
                                    : " applied"}
                                </p>
                              )}
                            </div>
                          </>
                        ) : (
                          <div className="flex flex-wrap items-center gap-2">
                            <span className="text-3xl font-semibold">${monthlyPrice}</span>
                            <span className="text-sm text-[#747474] dark:text-muted-foreground">
                              per month
                            </span>
                          </div>
                        )}
                      </div>

                      <Button
                        type="button"
                        variant={plan.popular && !isCurrentPlan ? "primary" : "outline"}
                        onClick={() =>
                          handleButtonClick(
                            product || {
                              id: plan.name.toLowerCase(),
                              name: plan.name,
                              description: plan.description,
                              active: true,
                              default_price: {
                                id: "",
                                currency: "usd",
                                unit_amount: 0,
                                recurring: {
                                  interval: "month",
                                  interval_count: 1
                                }
                              }
                            }
                          )
                        }
                        disabled={
                          loadingProductId === (product?.id || plan.name.toLowerCase()) ||
                          (useBitcoin && isTeamPlan) ||
                          (isIOSPlatform && !isFreeplan && product?.is_available === false)
                        }
                        className={cn(
                          "h-12 w-full gap-2 text-base",
                          !(plan.popular && !isCurrentPlan) && "bg-white/40 dark:bg-white/0",
                          useBitcoin && isTeamPlan && "cursor-not-allowed"
                        )}
                      >
                        {useBitcoin && isTeamPlan
                          ? "Not Available"
                          : getButtonText(
                              product || {
                                id: plan.name.toLowerCase(),
                                name: plan.name,
                                description: plan.description || "",
                                active: true,
                                default_price: {
                                  id: "",
                                  currency: "usd",
                                  unit_amount: 0,
                                  recurring: {
                                    interval: "month",
                                    interval_count: 1
                                  }
                                }
                              }
                            )}
                      </Button>
                    </div>
                  </div>
                );
              })}
            </div>
          );
        })()}

        <div className="mx-auto mt-8 flex w-full max-w-7xl flex-col items-center gap-6 px-4 sm:px-6 lg:px-8">
          {!isIOSPlatform && (
            <button
              type="button"
              onClick={() => navigate({ to: "/redeem" })}
              className="text-sm font-medium text-[#747474] underline underline-offset-4 decoration-neutral-900/20 transition-colors hover:text-[hsl(var(--maple-primary-strong))] hover:decoration-[hsl(var(--maple-primary-strong))] dark:text-muted-foreground dark:decoration-white/20 dark:hover:text-[hsl(var(--maple-primary))] dark:hover:decoration-[hsl(var(--maple-primary))]"
            >
              Have a subscription pass code?
            </button>
          )}
          <div className="flex flex-col items-center gap-3">
            <div className="inline-flex items-center gap-4 rounded-lg border border-neutral-900/10 bg-white/65 px-5 py-2.5 shadow-sm backdrop-blur dark:border-white/10 dark:bg-neutral-900/65">
              <div className="flex items-center gap-2 text-[hsl(var(--bitcoin))] text-sm font-medium">
                <Bitcoin className="h-4 w-4" />
                <span>Pay with Bitcoin</span>
              </div>
              <Switch
                id="bitcoin-toggle"
                checked={useBitcoin}
                onCheckedChange={setUseBitcoin}
                disabled={isGuestUser}
                className="scale-100 data-[state=checked]:bg-[hsl(var(--bitcoin))] data-[state=unchecked]:border-neutral-900/30 dark:data-[state=unchecked]:border-white/30"
              />
            </div>
            {isGuestUser && (
              <div className="text-sm font-medium text-maple-warning">
                Anonymous accounts must pay with Bitcoin (yearly only)
              </div>
            )}
          </div>
        </div>

        <PricingFAQ />
      </FullPageMain>
      <TeamSeatDialog
        open={showTeamSeatDialog}
        onOpenChange={setShowTeamSeatDialog}
        onConfirm={handleTeamSeatConfirm}
      />
      <ZapriteUpgradeDialog
        open={zapriteUpgradeOpen}
        onOpenChange={setZapriteUpgradeOpen}
        userId={os.auth.user?.user.id ?? null}
        targetProductId={zapriteUpgradeProductId}
        resumeUpgradeId={
          zapriteUpgradeProductId ? null : (freshBillingStatus?.pending_plan_change?.id ?? null)
        }
      />
      <VerificationModal />
    </>
  );
}

export const Route = createFileRoute("/pricing")({
  component: PricingPage,
  validateSearch: (search: Record<string, unknown>): PricingSearchParams => ({
    selected_plan: typeof search.selected_plan === "string" ? search.selected_plan : undefined,
    success:
      search.success === true || search.success === "true"
        ? true
        : search.success === false || search.success === "false"
          ? false
          : undefined,
    canceled:
      search.canceled === true || search.canceled === "true"
        ? true
        : search.canceled === false || search.canceled === "false"
          ? false
          : undefined
  })
});
