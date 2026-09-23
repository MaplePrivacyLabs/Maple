import { useBillingState } from "@/state/useLocalState";
import { formatResetDate } from "@/utils/dateFormat";

const CREDIT_NUMBER_FORMATTER = new Intl.NumberFormat("en-US");

function formatCredits(credits: number): string {
  return CREDIT_NUMBER_FORMATTER.format(credits);
}

function toPlanNameLabel(rawPlanName: string | undefined): string {
  if (!rawPlanName?.trim()) return "Loading...";
  const cleaned = (rawPlanName ?? "Pro").trim();
  const hasPlanSuffix = /\bplan\b/i.test(cleaned);
  return hasPlanSuffix ? cleaned : `${cleaned} Plan`;
}

type CreditUsageViewProps = {
  planLabel: string;
  percentUsed?: number;
  roundedUsed?: number;
  used?: number;
  apiBalance?: number;
  hasApiCredits: boolean;
  resetFullLabel?: string;
  formatCredits: (n: number) => string;
};

function CreditUsageView(p: CreditUsageViewProps) {
  const hasUsageMeter =
    p.percentUsed !== undefined && p.roundedUsed !== undefined && p.used !== undefined;

  return (
    <div
      className="w-full rounded-xl bg-[hsl(var(--sidebar-chrome))] p-2 transition-colors group-hover/credit-link:bg-[hsl(var(--sidebar-chrome-hover))]"
      title={p.resetFullLabel || undefined}
    >
      <div className="flex min-w-0 flex-wrap items-center gap-x-1.5 gap-y-0.5 text-[10px] leading-tight">
        <span className="inline-flex shrink-0 items-center rounded-full border border-border/50 bg-muted px-1.5 py-0.5 text-[9px] font-semibold uppercase tracking-wider text-foreground">
          {p.planLabel}
        </span>
        {hasUsageMeter ? (
          <>
            <span className="text-muted-foreground/50">·</span>
            <span className="shrink-0 font-semibold tabular-nums text-foreground">
              {p.roundedUsed}% used
            </span>
            {p.resetFullLabel && (
              <>
                <span className="text-muted-foreground/50">·</span>
                <span className="min-w-0 flex-1 truncate text-muted-foreground">
                  {p.resetFullLabel}
                </span>
              </>
            )}
          </>
        ) : null}
      </div>
      {hasUsageMeter ? (
        <div className="mt-1.5 min-h-0 rounded-sm py-1.5">
          <div className="h-[4px] w-full overflow-hidden rounded-full bg-[hsl(var(--sidebar-chrome-hover))]">
            <div
              className="h-full rounded-full transition-[width] duration-500 ease-out"
              style={{
                width: `${p.percentUsed}%`,
                background:
                  "linear-gradient(90deg, hsl(var(--maple-primary)), hsl(var(--maple-primary-strong)))"
              }}
            />
          </div>
          {p.hasApiCredits && (
            <div className="pt-1.5 text-[9.5px] leading-none text-muted-foreground">
              <span className="min-w-0 truncate tabular-nums text-[hsl(var(--maple-success))]">
                +{p.formatCredits(p.apiBalance ?? 0)} credits
              </span>
            </div>
          )}
        </div>
      ) : null}
    </div>
  );
}

export function CreditUsage() {
  const { billingStatus } = useBillingState();

  const totalLive = billingStatus?.total_tokens;
  const usedLive = billingStatus?.used_tokens;
  const hasUsageData = totalLive != null && totalLive > 0 && usedLive != null;
  const productName = billingStatus?.product_name;
  const apiBalance = billingStatus?.api_credit_balance;

  const used = hasUsageData ? Math.max(0, usedLive!) : undefined;
  const percentUsed = hasUsageData ? Math.min(100, Math.max(0, (used! / totalLive!) * 100)) : 0;

  const shouldShowUsageMeter = hasUsageData;

  const hasApiCredits = apiBalance !== undefined && apiBalance > 0;

  const planLabel = toPlanNameLabel(productName ?? undefined);
  const resetFullLabel = shouldShowUsageMeter
    ? formatResetDate(billingStatus?.usage_reset_date)
    : undefined;

  const props: CreditUsageViewProps = {
    planLabel,
    ...(shouldShowUsageMeter
      ? {
          percentUsed,
          roundedUsed: Math.round(percentUsed),
          used: used!
        }
      : {}),
    apiBalance,
    hasApiCredits,
    resetFullLabel,
    formatCredits
  };

  return <CreditUsageView {...props} />;
}
