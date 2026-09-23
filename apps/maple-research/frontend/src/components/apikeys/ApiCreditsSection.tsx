import { useState, useEffect } from "react";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Input } from "@/components/ui/input";
import { Loader2, CreditCard, Bitcoin, Coins, CheckCircle, Edit } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { getBillingService } from "@/billing/billingService";
import { useOpenSecret } from "@mapleai/sdk";
import { isIOS, isMobile, isTauri } from "@/utils/platform";
import {
  MIN_PURCHASE_CREDITS,
  MIN_PURCHASE_AMOUNT,
  type ApiCreditBalance
} from "@/billing/billingApi";

interface CreditPackage {
  credits: number;
  price: number;
  label: string;
}

const CREDIT_PACKAGES: CreditPackage[] = [
  { credits: 20000, price: 20, label: "20,000 credits" },
  { credits: 50000, price: 50, label: "50,000 credits" },
  { credits: 100000, price: 100, label: "100,000 credits" },
  { credits: 500000, price: 500, label: "500,000 credits" }
];

const CREDIT_NUMBER_FORMATTER = new Intl.NumberFormat("en-US");

function formatCredits(credits: number): string {
  return CREDIT_NUMBER_FORMATTER.format(credits);
}

function CreditBalanceCard({ balance }: { balance: number }) {
  return (
    <Card className="p-4">
      <div className="flex items-center justify-between">
        <div>
          <p className="text-sm text-muted-foreground">Extra Credit Balance</p>
          <p className="text-2xl font-bold flex items-center gap-2">
            <Coins className="h-5 w-5" />
            {formatCredits(balance)}
          </p>
          <p className="text-xs text-muted-foreground mt-1">
            {!isIOS() && "$1 per 1,000 credits • "}
            Extends your subscription when plan credits run out
          </p>
        </div>
      </div>
    </Card>
  );
}

interface ApiCreditsSectionProps {
  showSuccessMessage?: boolean;
}

export function ApiCreditsSection({ showSuccessMessage = false }: ApiCreditsSectionProps) {
  const [selectedPackage, setSelectedPackage] = useState<CreditPackage>(CREDIT_PACKAGES[0]);
  const [isPurchasing, setIsPurchasing] = useState(false);
  const [paymentMethod, setPaymentMethod] = useState<"stripe" | "zaprite" | null>(null);
  const [showSuccess, setShowSuccess] = useState(showSuccessMessage);
  const [showCustomAmount, setShowCustomAmount] = useState(false);
  const [customAmount, setCustomAmount] = useState("");
  const [purchaseError, setPurchaseError] = useState<string | null>(null);
  const { auth } = useOpenSecret();
  const isMobilePlatform = isMobile();

  const userEmail = auth.user?.user.email;
  const isGuestUser = auth.user?.user.login_method?.toLowerCase() === "guest";

  useEffect(() => {
    if (showSuccessMessage) {
      setShowSuccess(true);
      // Hide success message after 5 seconds
      const timer = setTimeout(() => setShowSuccess(false), 5000);
      return () => clearTimeout(timer);
    }
  }, [showSuccessMessage]);

  // Fetch credit balance
  const {
    data: creditBalance,
    isLoading: isLoadingBalance,
    error: balanceError
  } = useQuery<ApiCreditBalance, Error>({
    queryKey: ["apiCreditBalance"],
    queryFn: async () => {
      const billingService = getBillingService();
      return await billingService.getApiCreditBalance();
    },
    enabled: !!auth.user && !auth.loading
  });

  const handlePurchase = async (method: "stripe" | "zaprite") => {
    // Credits are consumed on iOS, but are not sold there in this IAP version.
    if (isIOS()) return;
    // Clear any previous errors
    setPurchaseError(null);

    // Validate custom amount if in custom mode
    let finalPackage = selectedPackage;
    if (showCustomAmount) {
      const amount = parseInt(customAmount);
      if (isNaN(amount) || amount < 10 || amount > 1000) {
        return; // Invalid amount, don't proceed
      }
      finalPackage = {
        credits: amount * 1000,
        price: amount,
        label: `${formatCredits(amount * 1000)} credits`
      };
    }

    setIsPurchasing(true);
    setPaymentMethod(method);

    try {
      const billingService = getBillingService();

      // Determine success/cancel URLs based on platform
      let successUrl: string;
      let cancelUrl: string | undefined;

      // For mobile platforms (iOS and Android), use Universal Links that match the AASA/App Links configuration
      if (isMobilePlatform) {
        successUrl = `https://trymaple.ai/payment-success-credits?source=${method}`;
        cancelUrl =
          method === "stripe" ? `https://trymaple.ai/payment-canceled?source=stripe` : undefined;
      } else if (isTauri()) {
        // For Tauri desktop, use trymaple.ai since tauri://localhost won't work in external browser
        successUrl = `https://trymaple.ai/?credits_success=true`;
        cancelUrl = method === "stripe" ? `https://trymaple.ai/` : undefined;
      } else {
        // For web, use regular URLs with query params
        const baseUrl = window.location.origin;
        successUrl = `${baseUrl}/?credits_success=true`;
        cancelUrl = method === "stripe" ? `${baseUrl}/` : undefined;
      }

      if (method === "stripe") {
        const response = await billingService.purchaseApiCredits({
          credits: finalPackage.credits,
          email: userEmail || "",
          success_url: successUrl,
          cancel_url: cancelUrl || successUrl
        });

        // For Tauri platforms (desktop and mobile), use opener plugin to open in external browser
        if (isTauri()) {
          const { invoke } = await import("@tauri-apps/api/core");
          await invoke("plugin:opener|open_url", { url: response.checkout_url }).catch(
            (err: Error) => {
              console.error("[Credits] Failed to open checkout with Tauri opener:", err);
              if (isMobile()) {
                throw new Error("Failed to open payment page in external browser.");
              } else {
                window.open(response.checkout_url, "_blank", "noopener");
              }
            }
          );
        } else {
          window.location.href = response.checkout_url;
        }
      } else {
        // For Zaprite, guest users pass empty string (backend infers from JWT)
        const response = await billingService.purchaseApiCreditsZaprite({
          credits: finalPackage.credits,
          email: userEmail || "",
          success_url: successUrl
        });

        // For Tauri platforms (desktop and mobile), use opener plugin to open in external browser
        if (isTauri()) {
          const { invoke } = await import("@tauri-apps/api/core");
          await invoke("plugin:opener|open_url", { url: response.checkout_url }).catch(
            (err: Error) => {
              console.error("[Credits] Failed to open checkout with Tauri opener:", err);
              if (isMobile()) {
                throw new Error("Failed to open payment page in external browser.");
              } else {
                window.open(response.checkout_url, "_blank", "noopener");
              }
            }
          );
        } else {
          window.location.href = response.checkout_url;
        }
      }
    } catch (error) {
      console.error("Failed to create checkout session:", error);
      setPurchaseError("Failed to create checkout session. Please try again.");
    } finally {
      setIsPurchasing(false);
      setPaymentMethod(null);
    }
  };

  if (isLoadingBalance) {
    return (
      <Card className="p-4">
        <div className="flex items-center justify-center py-4">
          <Loader2 className="h-6 w-6 animate-spin" />
        </div>
      </Card>
    );
  }

  if (balanceError) {
    return (
      <Card className="p-4">
        <p className="text-sm text-destructive">Failed to load credit balance</p>
      </Card>
    );
  }

  if (isIOS()) {
    return <CreditBalanceCard balance={creditBalance?.balance ?? 0} />;
  }

  return (
    <div className="space-y-4">
      {/* Success Message */}
      {showSuccess && (
        <Alert className="border-maple-success/40 bg-maple-success/10">
          <CheckCircle className="h-4 w-4 text-maple-success" />
          <AlertDescription className="text-sm">
            Payment successful! Your credits have been added to your account.
          </AlertDescription>
        </Alert>
      )}

      {/* Error Message */}
      {purchaseError && (
        <Alert className="border-destructive/50 bg-destructive/10">
          <AlertDescription className="text-sm text-destructive">{purchaseError}</AlertDescription>
        </Alert>
      )}

      {/* Current Balance */}
      <CreditBalanceCard balance={creditBalance?.balance ?? 0} />

      {/* Purchase Credits */}
      <Card className="p-4">
        <h3 className="font-medium mb-3">Purchase Credits</h3>

        {/* Credit Package Selection */}
        <div className="grid grid-cols-2 gap-2 mb-4">
          {CREDIT_PACKAGES.map((pkg) => (
            <button
              type="button"
              key={pkg.credits}
              onClick={() => {
                setSelectedPackage(pkg);
                setShowCustomAmount(false);
                setCustomAmount("");
              }}
              className={`p-3 rounded-lg border text-left transition-colors ${
                selectedPackage.credits === pkg.credits && !showCustomAmount
                  ? "border-primary bg-primary/10"
                  : "border-border hover:border-muted-foreground"
              }`}
            >
              <p className="font-medium text-sm">{pkg.label}</p>
              <p className="text-xs text-muted-foreground">${pkg.price}</p>
            </button>
          ))}
        </div>

        {/* Custom Amount Button */}
        <div className="mb-4">
          <button
            type="button"
            onClick={() => {
              setShowCustomAmount(!showCustomAmount);
              if (!showCustomAmount) {
                setCustomAmount("");
              }
            }}
            className={`w-full p-3 rounded-lg border text-left transition-colors flex items-center justify-between ${
              showCustomAmount
                ? "border-primary bg-primary/10"
                : "border-border hover:border-muted-foreground"
            }`}
          >
            <div>
              <p className="font-medium text-sm">Custom Amount</p>
              <p className="text-xs text-muted-foreground">$10 - $1,000</p>
            </div>
            <Edit className="h-4 w-4" />
          </button>

          {showCustomAmount && (
            <div className="mt-3 space-y-2">
              <Input
                type="number"
                placeholder="Enter amount ($10-$1000)"
                value={customAmount}
                onChange={(e) => setCustomAmount(e.target.value)}
                onKeyDown={(e) => {
                  // Prevent decimal point
                  if (e.key === ".") {
                    e.preventDefault();
                  }
                }}
                min="10"
                max="1000"
                step="1"
                className="text-center"
              />
              {customAmount && (
                <p className="text-xs text-center text-muted-foreground">
                  {(() => {
                    const amount = parseInt(customAmount);
                    if (isNaN(amount) || amount < 10) {
                      return "Minimum $10 required";
                    } else if (amount > 1000) {
                      return "Maximum $1,000 allowed";
                    } else {
                      return `${formatCredits(amount * 1000)} credits for $${amount}`;
                    }
                  })()}
                </p>
              )}
            </div>
          )}
        </div>

        {/* Payment Methods */}
        <div className="flex flex-col sm:flex-row gap-2">
          <Button
            onClick={() => handlePurchase("stripe")}
            disabled={
              isPurchasing ||
              isGuestUser ||
              (showCustomAmount &&
                (!customAmount || parseInt(customAmount) < 10 || parseInt(customAmount) > 1000))
            }
            className="flex-1 w-full sm:w-auto"
            size="default"
            title={isGuestUser ? "Anonymous accounts must use Bitcoin" : undefined}
          >
            {isPurchasing && paymentMethod === "stripe" ? (
              <Loader2 className="mr-2 h-4 w-4 animate-spin" />
            ) : (
              <CreditCard className="mr-2 h-4 w-4" />
            )}
            Pay with Card
          </Button>

          <Button
            onClick={() => handlePurchase("zaprite")}
            disabled={
              isPurchasing ||
              (showCustomAmount &&
                (!customAmount || parseInt(customAmount) < 10 || parseInt(customAmount) > 1000))
            }
            variant="outline"
            className="flex-1 w-full sm:w-auto"
            size="default"
          >
            {isPurchasing && paymentMethod === "zaprite" ? (
              <Loader2 className="mr-2 h-4 w-4 animate-spin" />
            ) : (
              <Bitcoin className="mr-2 h-4 w-4" />
            )}
            Pay with Bitcoin
          </Button>
        </div>

        <p className="text-xs text-muted-foreground mt-3">
          Minimum purchase: {MIN_PURCHASE_CREDITS.toLocaleString()} credits (${MIN_PURCHASE_AMOUNT})
        </p>
      </Card>
    </div>
  );
}
