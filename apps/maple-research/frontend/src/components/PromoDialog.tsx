import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Tag, Sparkles, Check, Cpu, Image, FileText, Mic, Globe, Zap } from "lucide-react";
import { useNavigate } from "@tanstack/react-router";
import type { DiscountResponse } from "@/billing/billingApi";
import { isIOS } from "@/utils/platform";

interface PromoDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  discount: DiscountResponse & { active: true };
}

const PROMO_BENEFITS = [
  {
    icon: <Cpu className="h-4 w-4" />,
    text: "Powerful AI models including Gemma 4 31B, GLM 5.3, and Kimi K2.6"
  },
  {
    icon: <Image className="h-4 w-4" />,
    text: "Image upload and vision analysis"
  },
  {
    icon: <FileText className="h-4 w-4" />,
    text: "Document processing (PDF, DOC, DOCX, TXT, MD)"
  },
  {
    icon: <Mic className="h-4 w-4" />,
    text: "Voice recording with Whisper transcription"
  },
  {
    icon: <Globe className="h-4 w-4" />,
    text: "Live web search powered by Brave"
  },
  {
    icon: <Zap className="h-4 w-4" />,
    text: "No daily limits - generous monthly usage"
  }
];

export function PromoDialog(props: PromoDialogProps) {
  // Web percentage discounts do not apply to StoreKit prices.
  if (isIOS()) return null;
  return <WebPromoDialog {...props} />;
}

function WebPromoDialog({ open, onOpenChange, discount }: PromoDialogProps) {
  const navigate = useNavigate();

  const handleUpgrade = () => {
    onOpenChange(false);
    navigate({ to: "/pricing" });
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-[calc(100vw-2rem)] sm:max-w-lg max-h-[calc(100vh-2rem)] overflow-y-auto">
        <DialogHeader>
          <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-3 mb-2">
            <div className="flex items-center gap-3">
              <div className="shrink-0 rounded-xl bg-gradient-to-br from-[hsl(var(--maple-primary)/0.2)] to-[hsl(var(--maple-primary-strong)/0.2)] p-2 sm:p-2.5">
                <Tag className="h-6 w-6 text-[hsl(var(--maple-primary))] sm:h-7 sm:w-7" />
              </div>
              <div>
                <DialogTitle className="text-lg sm:text-xl">{discount.name}</DialogTitle>
                <DialogDescription className="text-sm mt-0.5">Limited time offer</DialogDescription>
              </div>
            </div>
            <Badge className="w-fit bg-gradient-to-r from-[hsl(var(--maple-primary))] to-[hsl(var(--maple-primary-strong))] px-3 py-1 text-base text-[hsl(var(--maple-on-primary))] shadow-lg sm:py-1.5 sm:text-lg">
              {discount.percent_off}% OFF
            </Badge>
          </div>
        </DialogHeader>

        <div className="space-y-5 py-4">
          {/* Promo description */}
          <div className="rounded-xl border border-[hsl(var(--maple-primary)/0.2)] bg-gradient-to-r from-[hsl(var(--maple-primary)/0.1)] to-[hsl(var(--maple-primary-strong)/0.1)] p-4">
            <p className="text-base font-medium text-foreground">{discount.description}</p>
            {discount.duration_months && (
              <p className="text-sm text-muted-foreground mt-1">
                Discount applies for the first {discount.duration_months} month
                {discount.duration_months > 1 ? "s" : ""} of your subscription.
              </p>
            )}
          </div>

          {/* Benefits list */}
          <div className="space-y-3">
            <p className="text-sm font-medium text-muted-foreground">Upgrade to Pro and unlock:</p>
            <ul className="grid gap-2.5">
              {PROMO_BENEFITS.map((benefit) => (
                <li key={benefit.text} className="flex items-center gap-3 text-sm">
                  <div className="shrink-0 rounded-lg bg-maple-success/10 p-1.5 text-maple-success">
                    {benefit.icon}
                  </div>
                  <span>{benefit.text}</span>
                </li>
              ))}
            </ul>
          </div>

          {/* Privacy note */}
          <div className="pt-3 border-t">
            <div className="flex items-start gap-2 text-sm text-muted-foreground">
              <Check className="mt-0.5 h-4 w-4 shrink-0 text-maple-success" />
              <span>All features include end-to-end encryption. Your data stays private.</span>
            </div>
          </div>
        </div>

        <DialogFooter className="flex-col-reverse sm:flex-row gap-2 sm:gap-2">
          <Button
            variant="outline"
            onClick={() => onOpenChange(false)}
            className="w-full sm:w-auto"
          >
            Maybe Later
          </Button>
          <Button
            onClick={handleUpgrade}
            className="w-full gap-2 border-0 bg-gradient-to-r from-[hsl(var(--maple-primary))] to-[hsl(var(--maple-primary-strong))] text-[hsl(var(--maple-on-primary))] hover:brightness-110 sm:w-auto"
          >
            <Sparkles className="h-4 w-4" />
            Upgrade Now
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/**
 * Get the localStorage key for tracking if a promo has been seen
 */
export function getPromoSeenKey(promoName: string): string {
  // Normalize the promo name to create a consistent key
  const normalizedName = promoName.toLowerCase().replace(/[^a-z0-9]/g, "_");
  return `promo_seen_${normalizedName}`;
}

/**
 * Check if a promo has been seen
 */
export function hasSeenPromo(promoName: string): boolean {
  try {
    return localStorage.getItem(getPromoSeenKey(promoName)) === "true";
  } catch {
    return false;
  }
}

/**
 * Mark a promo as seen
 */
export function markPromoAsSeen(promoName: string): void {
  try {
    localStorage.setItem(getPromoSeenKey(promoName), "true");
  } catch {
    // Silently fail if localStorage is unavailable
  }
}
