import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useOpenSecret } from "@mapleai/sdk";
import { getBillingService } from "@/billing/billingService";
import type { UsageRange } from "@/billing/usageApi";
import { useUsageSummary } from "@/billing/useUsage";
import { SettingsPage, SettingsSection } from "@/components/settings/SettingsPage";
import { useBillingState } from "@/state/useLocalState";
import type { TeamStatus } from "@/types/team";
import { getTeamSeatCounts } from "@/utils/teamSeats";
import { CreditPlanCard, FreePlanCard, TeamPlanCard } from "./UsagePlanCard";
import { UsageExplorer } from "./UsageExplorer";
import { AboutNumbers, LastTwelveMonths, PricingTable, RecentRequests } from "./UsageSections";
import { ErrorBlock, LoadingBlock, ScopeNote } from "./ui";
import {
  browserTimeZone,
  buildPeriodProgress,
  localDate,
  statusWindow,
  summarizeDays,
  usagePlanKind
} from "./usageData";

const PAID_DESCRIPTION =
  "Credits and tokens your account used, by day, model and where requests came from.";
const FREE_DESCRIPTION = "Messages and tokens your account used, by day and model.";

export function UsageSettings() {
  const os = useOpenSecret();
  const accountId = os.auth.user?.user.id;
  const { billingStatus, setBillingStatus } = useBillingState();
  const [tz] = useState(browserTimeZone);
  const [range, setRange] = useState<UsageRange>("last_30_days");

  const statusQuery = useQuery({
    queryKey: ["billingStatus"],
    queryFn: async () => {
      const status = await getBillingService().getBillingStatus();
      setBillingStatus(status);
      return status;
    },
    enabled: !!accountId
  });
  const status = statusQuery.data ?? billingStatus;
  const kind = usagePlanKind(status);
  const planWindow = statusWindow(status);

  const usage = useUsageSummary(
    accountId,
    { range, tz, periodStart: planWindow?.start, periodEnd: planWindow?.end },
    { enabled: !!status }
  );
  // The Free limit counts UTC days, so its chart needs UTC buckets. When the
  // viewer is on UTC this is the same query as above.
  const freeUtc = useUsageSummary(
    accountId,
    { range: "last_30_days", tz: "UTC" },
    { enabled: !!status && kind === "free", keepPrevious: false }
  );
  const { data: teamStatus } = useQuery<TeamStatus>({
    queryKey: ["teamStatus"],
    queryFn: () => getBillingService().getTeamStatus(),
    enabled: !!accountId && kind === "team"
  });

  const now = new Date();
  const today = localDate(now, tz);
  const period = usage.data?.current_period ?? null;
  const used = Math.max(0, status?.used_tokens ?? 0);
  const allowance = status?.total_tokens ?? 0;
  const progress =
    planWindow && allowance > 0
      ? buildPeriodProgress({
          window: planWindow,
          days: period?.days ?? [],
          used,
          allowance,
          tz,
          now
        })
      : null;
  const you = period ? period.days.reduce((sum, d) => sum + d.plan_credits, 0) : null;
  const usedModels = useMemo(
    () =>
      usage.data
        ? summarizeDays(usage.data.days, usage.data.models).byModel.map(({ model, label }) => ({
            model,
            label
          }))
        : [],
    [usage.data]
  );

  if (!accountId) return null;

  if (!status) {
    return (
      <SettingsPage title="Usage" description={PAID_DESCRIPTION} wide>
        {statusQuery.isError ? (
          <ErrorBlock
            message="Couldn't load your plan."
            onRetry={() => void statusQuery.refetch()}
          />
        ) : (
          <SettingsSection>
            <LoadingBlock label="Loading usage..." />
          </SettingsSection>
        )}
      </SettingsPage>
    );
  }

  const dailyState = period ? "ready" : usage.isPending ? "loading" : "unavailable";

  return (
    <SettingsPage
      title="Usage"
      description={kind === "free" ? FREE_DESCRIPTION : PAID_DESCRIPTION}
      wide
      className="usage-viz"
    >
      <ScopeNote />
      {kind === "free" ? (
        <FreePlanCard
          status={status}
          utcRows={freeUtc.data?.days ?? []}
          utcState={freeUtc.data ? "ready" : freeUtc.isPending ? "loading" : "unavailable"}
          tz={tz}
          now={now}
        />
      ) : kind === "team" ? (
        <TeamPlanCard
          status={status}
          window={planWindow}
          progress={progress}
          tz={tz}
          you={you}
          teamStatus={teamStatus}
        />
      ) : (
        <CreditPlanCard
          status={status}
          window={planWindow}
          progress={progress}
          tz={tz}
          dailyState={dailyState}
        />
      )}
      <UsageExplorer
        kind={kind}
        range={range}
        onRangeChange={setRange}
        usage={{
          data: usage.data,
          isError: usage.isError,
          isPlaceholderData: usage.isPlaceholderData,
          refetch: () => void usage.refetch()
        }}
        today={today}
        periodStartDate={planWindow ? localDate(new Date(planWindow.start), tz) : null}
      />
      <RecentRequests accountId={accountId} kind={kind} tz={tz} />
      <PricingTable usedModels={usedModels} />
      {kind !== "free" && usage.data && (
        <LastTwelveMonths months={usage.data.months} currentMonth={today.slice(0, 7)} />
      )}
      <AboutNumbers
        kind={kind}
        planName={status.product_name}
        allowance={allowance}
        seats={getTeamSeatCounts(teamStatus).billedSeatCount}
        tz={tz}
      />
    </SettingsPage>
  );
}
