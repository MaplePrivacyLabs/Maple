import { useCallback, useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Loader2 } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { getBillingService } from "@/billing/billingService";
import { openValidatedCheckoutUrl } from "@/billing/billingApi";
import {
  formatUsdFromCents,
  isZapriteUpgradeTerminal,
  type ZapriteUpgradeQuote,
  type ZapriteUpgradeStatusResponse
} from "@/billing/zapriteUpgrade";
import { isAndroid, isIOS, isMobile, isTauri } from "@/utils/platform";

type ZapriteUpgradeDialogProps = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  userId: string | null;
  targetProductId: string | null;
  resumeUpgradeId?: string | null;
};

function zapriteSuccessUrl(): string {
  if (isMobile()) {
    return "https://trymaple.ai/payment-success?source=zaprite";
  }
  if (isTauri()) {
    return "https://trymaple.ai/pricing?success=true";
  }
  return `${window.location.origin}/pricing?success=true`;
}

function statusCopy(status: string, planName?: string): string {
  switch (status) {
    case "PENDING":
    case "PROCESSING":
    case "PAID":
    case "OVERPAID":
      return "Waiting for payment.";
    case "UNDERPAID":
      return "Payment is short.";
    case "COMPLETED":
      return planName ? `You're on ${planName}.` : "Upgrade complete.";
    case "EXPIRED":
      return "This quote expired.";
    case "CANCELED":
    case "FAILED":
      return "Upgrade could not be completed.";
    case "PAID_NEEDS_REVIEW":
    case "PAID_UNFULFILLABLE":
      return "Payment received. Contact support if your plan doesn't update.";
    case "REFUNDED":
    case "REVOKED":
      return "This upgrade is no longer active.";
    default:
      return "Preparing checkout.";
  }
}

export function ZapriteUpgradeDialog(props: ZapriteUpgradeDialogProps) {
  if (isAndroid()) return null;
  return <WebZapriteUpgradeDialog {...props} />;
}

function WebZapriteUpgradeDialog({
  open,
  onOpenChange,
  userId,
  targetProductId,
  resumeUpgradeId
}: ZapriteUpgradeDialogProps) {
  const queryClient = useQueryClient();
  const [quote, setQuote] = useState<ZapriteUpgradeQuote | null>(null);
  const [upgradeId, setUpgradeId] = useState<string | null>(resumeUpgradeId ?? null);
  const [idempotencyKey] = useState(() => crypto.randomUUID());
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const mountedUserId = useRef(userId);

  useEffect(() => {
    mountedUserId.current = userId;
  }, [userId]);

  useEffect(() => {
    if (!open) {
      return;
    }
    setError(null);
    if (resumeUpgradeId) {
      setUpgradeId(resumeUpgradeId);
      return;
    }
    if (!targetProductId) {
      return;
    }
    let cancelled = false;
    setQuote(null);
    getBillingService()
      .createZapriteUpgradeQuote(targetProductId)
      .then((nextQuote) => {
        if (!cancelled && mountedUserId.current === userId) {
          setQuote(nextQuote);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : "Failed to load upgrade quote");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [open, resumeUpgradeId, targetProductId, userId]);

  const { data: upgradeStatus } = useQuery<ZapriteUpgradeStatusResponse>({
    queryKey: ["zapriteUpgrade", userId, upgradeId],
    queryFn: async () => {
      if (!upgradeId) {
        throw new Error("Missing upgrade id");
      }
      return getBillingService().getZapriteUpgradeStatus(upgradeId);
    },
    enabled: open && !!userId && !!upgradeId,
    refetchInterval: (query) => {
      const status = query.state.data?.status;
      if (!status || isZapriteUpgradeTerminal(status)) {
        return false;
      }
      return 2000;
    }
  });

  useEffect(() => {
    if (upgradeStatus?.status === "COMPLETED" && mountedUserId.current === userId) {
      void queryClient.invalidateQueries({ queryKey: ["billingStatus"] });
    }
  }, [upgradeStatus?.status, queryClient, userId]);

  const displayedQuote = upgradeStatus?.quote ?? quote;

  const handleConfirm = useCallback(async () => {
    if (!displayedQuote || isIOS()) {
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      const created = await getBillingService().createZapriteUpgrade(
        displayedQuote.quote_id,
        idempotencyKey,
        zapriteSuccessUrl()
      );
      if (mountedUserId.current !== userId) {
        return;
      }
      setUpgradeId(created.upgrade_id);
      await openValidatedCheckoutUrl(created.checkout_url);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to start Bitcoin checkout");
    } finally {
      setSubmitting(false);
    }
  }, [displayedQuote, idempotencyKey, userId]);

  const currentStatus = upgradeStatus?.status;
  const canConfirm = !!displayedQuote && !upgradeId && !isIOS() && !submitting;
  const checkoutUrl = upgradeStatus?.checkout_url;
  const canReopenCheckout =
    !!checkoutUrl && !isIOS() && !!currentStatus && !isZapriteUpgradeTerminal(currentStatus);
  const targetName = displayedQuote?.target.plan_name ?? "Max";

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Upgrade to {targetName}</DialogTitle>
          <DialogDescription className="sr-only">
            Confirm your Bitcoin upgrade to {targetName}.
          </DialogDescription>
        </DialogHeader>

        {error && <p className="text-sm text-maple-error">{error}</p>}

        {!displayedQuote && !error && (
          <div className="flex items-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="h-4 w-4 animate-spin" />
            Loading...
          </div>
        )}

        {displayedQuote && !currentStatus && (
          <div className="space-y-1">
            <p className="text-2xl font-semibold">
              {formatUsdFromCents(displayedQuote.amount_due_cents)}
            </p>
            <p className="text-sm text-muted-foreground">
              Renews {new Date(displayedQuote.subscription_end).toLocaleDateString()}
            </p>
          </div>
        )}

        {currentStatus && (
          <p className="text-sm text-muted-foreground">
            {statusCopy(currentStatus, displayedQuote?.target.plan_name)}
          </p>
        )}

        {isIOS() && (
          <p className="text-sm text-muted-foreground">
            Bitcoin upgrades aren't available in the iOS app.
          </p>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            {currentStatus === "COMPLETED" ? "Done" : "Cancel"}
          </Button>
          {canConfirm && (
            <Button variant="primary" onClick={handleConfirm} disabled={submitting}>
              {submitting ? <Loader2 className="h-4 w-4 animate-spin" /> : "Pay with Bitcoin"}
            </Button>
          )}
          {canReopenCheckout && checkoutUrl && (
            <Button
              variant="primary"
              onClick={() => {
                void openValidatedCheckoutUrl(checkoutUrl).catch((err: unknown) => {
                  setError(err instanceof Error ? err.message : "Failed to open checkout");
                });
              }}
            >
              Continue to payment
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
