import { useEffect, useRef, useState } from "react";
import { ArrowLeft, Check, Loader2 } from "lucide-react";
import type { BillingStatus } from "@/billing/billingApi";
import {
  AppleBillingSessionChangedError,
  type AppleBillingPurchaseResult
} from "@/billing/appleBillingSession";
import { AppleBillingApiError } from "@/billing/appleBillingApi";
import { appleBillingErrorMessage } from "@/billing/appleBillingLifecycle";
import {
  applePurchaseConflict,
  currentApplePlan,
  type ApplePlan
} from "@/billing/applePricingPolicy";
import { StoreKitRecoveryError, type StoreKitProduct } from "@/services/storeKitService";
import { Button } from "@/components/ui/button";

interface ApplePricingProps {
  userId: string | null;
  isGuest: boolean;
  status: BillingStatus | undefined;
  statusLoading: boolean;
  statusError: boolean;
  ready: boolean;
  busy: boolean;
  recoveryError: string | null;
  products: Partial<Record<ApplePlan, StoreKitProduct>>;
  productsLoading: boolean;
  productsError: string | null;
  purchase: (productId: string) => Promise<AppleBillingPurchaseResult>;
  restore: () => Promise<unknown[]>;
  retry: () => Promise<void>;
  reloadPrices: () => void;
  refreshStatus: () => void;
  manageApple: () => Promise<void>;
  openEula: () => Promise<void>;
  navigate: (destination: "/" | "/signup" | "/settings/billing" | "/privacy" | "/terms") => void;
}

const planFeatures: Record<ApplePlan, string[]> = {
  Pro: ["Generous monthly usage", "All AI models", "Image and document uploads", "Voice input"],
  Max: ["10× Pro's monthly credits", "All Pro features", "Priority support"]
};

/** The iOS paywall never renders or invokes another provider's checkout. */
export function ApplePricing(props: ApplePricingProps) {
  const [operation, setOperation] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const feedbackRef = useRef<HTMLParagraphElement>(null);
  const [pendingPlan, setPendingPlan] = useState<ApplePlan | null>(null);
  const operationLock = useRef(false);
  const active = useRef(true);
  const lifetime = useRef(0);
  const currentPurchase = useRef(props.purchase);
  currentPurchase.current = props.purchase;
  useEffect(() => {
    active.current = true;
    const generation = lifetime.current + 1;
    lifetime.current = generation;
    return () => {
      active.current = false;
      lifetime.current = generation + 1;
    };
  }, []);
  useEffect(() => {
    // Restore/management controls can be below the fold. Bring only an explicit
    // action's result into view; background recovery updates must not move the page.
    if (message || error) feedbackRef.current?.scrollIntoView({ block: "nearest" });
  }, [message, error]);

  const conflict = applePurchaseConflict(props.status);
  const working = props.busy || operation !== null;
  const awaitingApproval = pendingPlan !== null && !currentApplePlan(props.status, pendingPlan);
  const purchasesEnabled = props.status?.ios_iap_enabled === true;

  async function run(action: string, work: (isCurrent: () => boolean) => Promise<string>) {
    if (working || operationLock.current) return;
    operationLock.current = true;
    const owner = props.purchase;
    const generation = lifetime.current;
    const isCurrent = () =>
      active.current && lifetime.current === generation && currentPurchase.current === owner;
    setOperation(action);
    setMessage(null);
    setError(null);
    try {
      const result = await work(isCurrent);
      if (isCurrent()) setMessage(result);
    } catch (failure) {
      if (isCurrent()) {
        setError(
          action === "eula"
            ? "The Apple license agreement could not be opened. Please try again."
            : action === "manage"
              ? "Apple subscription management could not be opened. You can also manage subscriptions in your device's Settings."
              : failure instanceof AppleBillingApiError ||
                  failure instanceof StoreKitRecoveryError ||
                  failure instanceof AppleBillingSessionChangedError
                ? appleBillingErrorMessage(failure)
                : action === "restore"
                  ? "Purchases could not be restored. Try Restore purchases again and complete Apple's sign-in prompt if asked."
                  : "Your purchase could not be confirmed. Do not purchase again; use Retry purchases when connected."
        );
      }
    } finally {
      operationLock.current = false;
      if (active.current) setOperation(null);
    }
  }

  const manage = () =>
    void run("manage", async (isCurrent) => {
      await props.manageApple();
      if (isCurrent()) props.refreshStatus();
      return "Subscription changes can take a moment to appear. Use Retry purchases to refresh.";
    });

  return (
    <div className="mx-auto w-full max-w-3xl space-y-6">
      <Button variant="ghost" onClick={() => props.navigate("/")} className="gap-2">
        <ArrowLeft className="h-4 w-4" /> {props.userId ? "Chat" : "Home"}
      </Button>
      <header className="space-y-3 text-center">
        <h1 className="font-displayWide text-3xl">Choose your Maple plan</h1>
        <p className="text-muted-foreground">Private AI chat, with more room to explore.</p>
        <p className="text-sm text-muted-foreground">Monthly subscriptions billed by Apple.</p>
      </header>

      {props.isGuest && props.userId && (
        <div className="space-y-2 rounded-lg border border-maple-warning/40 bg-maple-warning/10 p-4 text-sm">
          <p>
            Your purchase is linked to this anonymous Maple account. Save your Account ID and the
            password you created so you can sign in again. An Apple purchase cannot recover a lost
            Maple login.
          </p>
          <p className="break-all font-mono text-xs">Account ID: {props.userId}</p>
        </div>
      )}

      {!props.userId && (
        <p className="text-center text-sm">
          Sign in or create a Maple account before subscribing or restoring.
        </p>
      )}
      {props.statusLoading && props.userId && (
        <p role="status" className="text-center text-sm">
          Checking your current plan…
        </p>
      )}
      {props.statusError && (
        <div role="alert" className="space-y-2 rounded-lg border p-4 text-sm">
          <p>Your current plan could not be checked. Reload it before starting a new purchase.</p>
          <Button variant="outline" onClick={props.refreshStatus}>
            Reload current plan
          </Button>
        </div>
      )}
      {conflict && (
        <div className="space-y-3 rounded-lg border p-4 text-sm">
          <p>{conflict}</p>
          <Button variant="outline" onClick={() => props.navigate("/settings/billing")}>
            Review billing
          </Button>
        </div>
      )}
      {props.userId && props.status && !purchasesEnabled && (
        <p className="text-center text-sm text-muted-foreground">
          New Apple purchases are temporarily unavailable. You can still restore and manage existing
          subscriptions.
        </p>
      )}
      {(error || props.recoveryError) && (
        <p
          ref={error ? feedbackRef : undefined}
          role="alert"
          className="rounded-lg border border-destructive/40 p-4 text-sm"
        >
          {error ?? props.recoveryError}
        </p>
      )}
      {message && (
        <p ref={feedbackRef} role="status" className="rounded-lg border p-4 text-sm">
          {message}
        </p>
      )}

      <div className="grid gap-4 sm:grid-cols-2">
        {(["Pro", "Max"] as const).map((plan) => {
          const product = props.products[plan];
          const current = currentApplePlan(props.status, plan);
          const disabled = current
            ? working
            : !!props.userId &&
              (working ||
                awaitingApproval ||
                !!props.recoveryError ||
                !props.ready ||
                !purchasesEnabled ||
                !product ||
                !!conflict ||
                props.statusLoading ||
                props.statusError ||
                !props.status);
          return (
            <section key={plan} className="flex flex-col gap-5 rounded-xl border bg-card p-5">
              <h2 className="text-2xl font-semibold">Maple {plan}</h2>
              <p className="min-h-8 text-2xl font-semibold">
                {product ? (
                  <>
                    {product.formattedPrice}
                    <span className="text-sm font-normal text-muted-foreground"> / month</span>
                  </>
                ) : (
                  <span className="text-sm text-muted-foreground">
                    {props.productsLoading ? "Loading App Store price…" : "Price unavailable"}
                  </span>
                )}
              </p>
              <ul className="flex-1 space-y-2 text-sm">
                {planFeatures[plan].map((feature) => (
                  <li key={feature} className="flex gap-2">
                    <Check className="h-4 w-4 shrink-0" />
                    {feature}
                  </li>
                ))}
              </ul>
              <Button
                variant="primary"
                disabled={disabled}
                onClick={() => {
                  if (!props.userId) {
                    props.navigate("/signup");
                    return;
                  }
                  if (current) {
                    manage();
                    return;
                  }
                  if (disabled || !product) return;
                  void run(plan, async (isCurrent) => {
                    const result = await props.purchase(product.productId);
                    if (result.status !== "success") {
                      if (result.status === "cancelled")
                        return "Purchase cancelled. No changes were made.";
                      if (isCurrent()) setPendingPlan(plan);
                      return "Your purchase is awaiting Apple's approval. Maple will update when it is approved; do not purchase again.";
                    }
                    return currentApplePlan(result.acknowledgement, plan)
                      ? `Purchase confirmed. Maple ${plan} is your current plan.`
                      : "Purchase confirmed. Review billing to see your current plan and manage your subscriptions.";
                  });
                }}
              >
                {operation === plan && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                {!props.userId
                  ? "Sign in to subscribe"
                  : current
                    ? "Manage Apple subscription"
                    : `Subscribe to ${plan}`}
              </Button>
            </section>
          );
        })}
      </div>

      {props.productsError && (
        <p role="alert" className="text-sm text-muted-foreground">
          {props.productsError}
        </p>
      )}
      <div className="flex flex-wrap justify-center gap-3">
        <Button variant="outline" onClick={props.reloadPrices}>
          Reload prices
        </Button>
        <Button
          variant="outline"
          disabled={working || !props.ready}
          onClick={() =>
            void run("restore", async (isCurrent) => {
              const restored = await props.restore();
              if (isCurrent()) props.refreshStatus();
              return restored.length
                ? "Purchases restored. Review billing to see your current plan."
                : "No purchases were found for this Apple Account.";
            })
          }
        >
          Restore purchases
        </Button>
        <Button
          variant="outline"
          disabled={working || !props.ready}
          onClick={() =>
            void run("retry", async (isCurrent) => {
              await props.retry();
              if (isCurrent()) props.refreshStatus();
              return "Purchases checked. Your confirmed plan is up to date.";
            })
          }
        >
          Retry purchases
        </Button>
        <Button variant="outline" disabled={working} onClick={manage}>
          Manage Apple subscriptions
        </Button>
      </div>
      <div className="space-y-3 text-center text-xs leading-relaxed text-muted-foreground">
        <p>
          Payment is charged to your Apple Account at confirmation. Subscriptions renew
          automatically each month unless cancelled at least 24 hours before the current period
          ends. Apple charges for renewal within 24 hours before the end of the current period.
          Manage or cancel in your App Store subscription settings. Cancelling stops the next
          renewal; access continues through the paid period.
        </p>
        <div className="flex justify-center gap-4">
          <button className="underline" onClick={() => props.navigate("/terms")}>
            Terms of Service
          </button>
          <button className="underline" onClick={() => props.navigate("/privacy")}>
            Privacy Policy
          </button>
          <button
            className="underline"
            disabled={working}
            onClick={() =>
              void run("eula", async () => {
                await props.openEula();
                return "";
              })
            }
          >
            Apple Standard EULA
          </button>
        </div>
      </div>
      <Button className="w-full" variant="ghost" onClick={() => props.navigate("/")}>
        Continue with current plan
      </Button>
    </div>
  );
}
