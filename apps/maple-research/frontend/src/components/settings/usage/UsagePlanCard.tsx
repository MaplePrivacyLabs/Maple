import type { ReactNode } from "react";
import { Link } from "@tanstack/react-router";
import { AlertTriangle, Coins, TrendingUp, UsersRound } from "lucide-react";
import type { BillingStatus } from "@/billing/billingApi";
import type { UsageDayRow } from "@/billing/usageApi";
import { SettingsSection } from "@/components/settings/SettingsPage";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import type { TeamStatus } from "@/types/team";
import { isAndroid, isMobile } from "@/utils/platform";
import {
  formatTeamSeatMismatchMessage,
  getTeamSeatCounts,
  getTeamSeatMismatch
} from "@/utils/teamSeats";
import { CumulativeChart, Legend, Meter, StackedBars, type StackDatum } from "./charts";
import { LoadingBlock } from "./ui";
import {
  buildFreeDays,
  fmt,
  fmtCompact,
  fmtDay,
  fmtRange,
  fmtWeekday,
  FREE_DAILY_MESSAGES,
  freeResetLabel,
  freeUsedToday,
  localDate,
  type PeriodProgress
} from "./usageData";

const CHART_HEIGHT = 250;

function PlanBadge({ name }: { name: string }) {
  return (
    <Badge variant="secondary" className="h-5 px-1.5 text-[10px] uppercase tracking-wider">
      {name}
    </Badge>
  );
}

function resetsIn(daysLeft: number): string {
  if (daysLeft <= 0) return "today";
  return daysLeft === 1 ? "in 1 day" : `in ${daysLeft} days`;
}

type PaidCardProps = {
  status: BillingStatus;
  window: { start: string; end: string } | null;
  progress: PeriodProgress | null;
  tz: string;
};

function periodLabel(window: PaidCardProps["window"], tz: string) {
  if (!window) return null;
  return fmtRange(localDate(new Date(window.start), tz), localDate(new Date(window.end), tz));
}

/** Pro, Max and other credit plans: plan meter, cumulative chart and Extra credits. */
export function CreditPlanCard({
  status,
  window,
  progress,
  tz,
  dailyState
}: PaidCardProps & { dailyState: "ready" | "loading" | "unavailable" }) {
  const used = Math.max(0, status.used_tokens ?? 0);
  const allowance = status.total_tokens ?? 0;
  const balance = status.api_credit_balance ?? 0;
  const resetDay = window ? fmtDay(localDate(new Date(window.end), tz)) : null;
  const range = periodLabel(window, tz);
  const left = Math.max(0, allowance - used);

  return (
    <SettingsSection>
      <div className="flex flex-col gap-6 lg:flex-row">
        <div className="lg:w-64 lg:shrink-0">
          <div className="flex items-center justify-between gap-2">
            <p className="text-xs font-medium text-muted-foreground">
              This billing period{range ? ` · ${range}` : ""}
            </p>
            {status.product_name && <PlanBadge name={status.product_name} />}
          </div>
          <p className="mt-2 text-4xl font-semibold tracking-tight">{fmt(used)}</p>
          <p className="text-sm text-muted-foreground">
            {allowance > 0 ? `of ${fmt(allowance)} plan credits` : "plan credits used"}
          </p>
          {allowance > 0 && (
            <>
              <div className="mt-4">
                <Meter
                  value={used}
                  max={allowance}
                  height={10}
                  over={used >= allowance}
                  label="Plan credits used"
                />
              </div>
              <p className="mt-2 text-xs text-muted-foreground">
                {used >= allowance ? "Plan credits used up" : `${fmt(left)} left`}
                {resetDay && progress && ` · resets ${resetDay} (${resetsIn(progress.daysLeft)})`}
              </p>
            </>
          )}
          {progress && (
            <PaceNote>
              {progress.used === 0 ? (
                "No plan credits used yet this period."
              ) : progress.exhausted ? (
                <>
                  Plan credits ran out.{" "}
                  {balance > 0
                    ? `Extra credits cover usage until the reset on ${resetDay}.`
                    : `Add Extra credits to keep going before the reset on ${resetDay}.`}
                </>
              ) : progress.runOutDate ? (
                <>
                  About {fmt(progress.pace)}{" "}
                  {Math.round(progress.pace) === 1 ? "credit" : "credits"} a day reaches{" "}
                  {fmtCompact(allowance)} around{" "}
                  <span className="font-medium text-foreground">{fmtDay(progress.runOutDate)}</span>
                  .{" "}
                  {balance > 0
                    ? "After that, Extra credits cover the rest."
                    : "After that, you need Extra credits until the reset."}
                </>
              ) : (
                <>
                  About {fmt(progress.pace)}{" "}
                  {Math.round(progress.pace) === 1 ? "credit" : "credits"} a day comes to roughly{" "}
                  <span className="font-medium text-foreground">
                    {fmtCompact(progress.projected)}
                  </span>{" "}
                  of {fmtCompact(allowance)} by {resetDay}.
                </>
              )}
            </PaceNote>
          )}
        </div>
        <div className="min-w-0 flex-1">
          {progress && dailyState === "ready" ? (
            <>
              <CumulativeChart
                actual={progress.actual}
                projected={progress.projected}
                totalDays={progress.dates.length}
                allowance={allowance}
                allowanceLabel="Plan"
                overLabel="Above the line comes from Extra credits"
                ticks={progress.ticks.map((index) => ({
                  index,
                  label: fmtDay(progress.dates[index])
                }))}
                height={CHART_HEIGHT}
                format={fmtCompact}
              />
              <p className="mt-2 text-xs text-muted-foreground">
                Solid line: credits used so far. Dashed line: projection at the average daily pace.
              </p>
            </>
          ) : dailyState === "loading" ? (
            <LoadingBlock label="Loading this period..." height={CHART_HEIGHT} />
          ) : (
            <div
              className="flex items-center justify-center rounded-lg border border-dashed text-sm text-muted-foreground"
              style={{ minHeight: CHART_HEIGHT }}
            >
              The daily chart for this period isn't available right now.
            </div>
          )}
        </div>
      </div>
      <div className="mt-5 flex flex-col gap-3 border-t pt-4 sm:flex-row sm:items-center sm:justify-between">
        <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-sm">
          <Coins className="h-4 w-4 shrink-0 text-muted-foreground" />
          <span className="font-medium">{fmt(balance)} Extra credits</span>
          <span className="text-muted-foreground">
            · used after plan credits run out, and they don't expire
          </span>
        </p>
        {!isMobile() && (
          <Button asChild variant="outline" size="sm" className="h-8 shrink-0">
            <Link to="/settings/api">Buy credits</Link>
          </Button>
        )}
      </div>
    </SettingsSection>
  );
}

function PaceNote({ children }: { children: ReactNode }) {
  return (
    <div className="mt-5 rounded-lg bg-muted/60 p-3 text-xs leading-relaxed text-muted-foreground">
      <p className="mb-1 flex items-center gap-1.5 font-medium text-foreground">
        <TrendingUp className="h-3.5 w-3.5" /> At the current pace
      </p>
      {children}
    </div>
  );
}

/** Team: the shared pool from status, with your own share from usage history. */
export function TeamPlanCard({
  status,
  window,
  progress,
  tz,
  you,
  teamStatus
}: PaidCardProps & { you: number | null; teamStatus: TeamStatus | undefined }) {
  const used = Math.max(0, status.used_tokens ?? 0);
  const allowance = status.total_tokens ?? 0;
  const mine = you === null ? null : Math.min(you, used);
  const rest = mine === null ? null : Math.max(0, used - mine);
  const resetDay = window ? fmtDay(localDate(new Date(window.end), tz)) : null;
  const range = periodLabel(window, tz);
  const seats = getTeamSeatCounts(teamStatus).billedSeatCount;
  const mismatch = getTeamSeatMismatch(teamStatus);
  const pct = (n: number) => (allowance > 0 ? Math.min(100, (n / allowance) * 100) : 0);

  return (
    <SettingsSection>
      <div className="flex flex-col gap-6 lg:flex-row">
        <div className="lg:w-72 lg:shrink-0">
          <div className="flex items-center justify-between gap-2">
            <p className="text-xs font-medium text-muted-foreground">
              Team pool{range ? ` · ${range}` : ""}
            </p>
            {status.product_name && <PlanBadge name={status.product_name} />}
          </div>
          <p className="mt-2 text-4xl font-semibold tracking-tight">{fmt(used)}</p>
          <p className="text-sm text-muted-foreground">of {fmt(allowance)} team credits</p>
          <div
            className="mt-4 flex h-2.5 w-full gap-[2px] overflow-hidden rounded-full bg-[hsl(var(--sidebar-chrome-hover))]"
            role="meter"
            aria-label="Team credits used"
            aria-valuemin={0}
            aria-valuemax={allowance}
            aria-valuenow={Math.min(used, allowance)}
          >
            {mine === null || rest === null ? (
              <div
                className="h-full rounded-full"
                style={{ width: `${pct(used)}%`, background: "var(--pool-plan)" }}
              />
            ) : (
              <>
                <div
                  className="h-full rounded-l-full"
                  style={{ width: `${pct(mine)}%`, background: "var(--src-app)" }}
                />
                <div
                  className="h-full"
                  style={{ width: `${pct(rest)}%`, background: "var(--pool-plan)" }}
                />
              </>
            )}
          </div>
          <div className="mt-2.5 space-y-1 text-xs">
            {mine !== null && rest !== null && (
              <Legend
                items={[
                  { key: "me", label: "You", color: "var(--src-app)", value: fmt(mine) },
                  {
                    key: "rest",
                    label: "Rest of team",
                    color: "var(--pool-plan)",
                    value: fmt(rest)
                  }
                ]}
              />
            )}
            <p className="pt-1 text-muted-foreground">
              {seats && seats > 0
                ? `${seats} ${seats === 1 ? "seat" : "seats"} × ${fmt(allowance / seats)}, shared.`
                : "Shared by the whole team."}
              {resetDay && ` Resets ${resetDay}.`}
            </p>
          </div>
        </div>
        <div className="min-w-0 flex-1 space-y-3">
          {mismatch && (
            <p className="flex items-start gap-2 rounded-lg border border-destructive/30 bg-destructive/5 p-3 text-xs leading-relaxed text-foreground">
              <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-destructive" />
              {formatTeamSeatMismatchMessage(
                mismatch,
                teamStatus?.role === "admin" || teamStatus?.is_team_admin ? "admin" : "member"
              )}
            </p>
          )}
          {progress && (
            <PaceNote>
              {progress.used === 0 ? (
                "No team credits used yet this period."
              ) : progress.exhausted ? (
                `The team pool ran out. Team usage pauses until the reset on ${resetDay}.`
              ) : progress.runOutDate ? (
                <>
                  About {fmt(progress.pace)}{" "}
                  {Math.round(progress.pace) === 1 ? "credit" : "credits"} a day reaches{" "}
                  {fmtCompact(allowance)} around{" "}
                  <span className="font-medium text-foreground">{fmtDay(progress.runOutDate)}</span>
                  . After that, team usage pauses until the reset.
                </>
              ) : (
                <>
                  About {fmt(progress.pace)}{" "}
                  {Math.round(progress.pace) === 1 ? "credit" : "credits"} a day comes to roughly{" "}
                  <span className="font-medium text-foreground">
                    {fmtCompact(progress.projected)}
                  </span>{" "}
                  of {fmtCompact(allowance)} by {resetDay}.
                </>
              )}
            </PaceNote>
          )}
        </div>
      </div>
      <div className="mt-5 border-t pt-4">
        <p className="flex items-center gap-2 text-sm text-muted-foreground">
          <UsersRound className="h-4 w-4 shrink-0" />
          Everything below is your own usage. The pool above includes the whole team.
        </p>
      </div>
    </SettingsSection>
  );
}

/** Free: messages today against the per-UTC-day limit. */
export function FreePlanCard({
  status,
  utcRows,
  utcState,
  tz,
  now
}: {
  status: BillingStatus;
  utcRows: UsageDayRow[];
  utcState: "ready" | "loading" | "unavailable";
  tz: string;
  now: Date;
}) {
  const usedToday = freeUsedToday(status.chats_remaining);
  const days = buildFreeDays({ rows: utcRows, now, usedToday });
  const data: StackDatum[] = days.map((d) => ({
    key: d.date,
    tick: fmtDay(d.date),
    title: fmtWeekday(d.date),
    values: { requests: d.requests },
    partial: d.partial
  }));

  return (
    <SettingsSection>
      <div className="flex flex-col gap-6 lg:flex-row">
        <div className="lg:w-64 lg:shrink-0">
          <div className="flex items-center justify-between gap-2">
            <p className="text-xs font-medium text-muted-foreground">Today</p>
            <PlanBadge name={status.product_name || "Free"} />
          </div>
          <p className="mt-2 text-4xl font-semibold tracking-tight">{fmt(usedToday)}</p>
          <p className="text-sm text-muted-foreground">of {FREE_DAILY_MESSAGES} messages</p>
          <div className="mt-4">
            <Meter
              value={usedToday}
              max={FREE_DAILY_MESSAGES}
              height={10}
              over={usedToday >= FREE_DAILY_MESSAGES}
              label="Messages used today"
            />
          </div>
          <p className="mt-2 text-xs text-muted-foreground">{freeResetLabel(now, tz)}</p>
          <p className="mt-4 text-xs leading-relaxed text-muted-foreground">
            The Free plan counts messages per day instead of credits. Paid plans add every model,
            the API and monthly credits.
          </p>
          {!isAndroid() && (
            <Button asChild variant="primary" size="sm" className="mt-4 h-8">
              <Link to="/pricing">See plans</Link>
            </Button>
          )}
        </div>
        <div className="min-w-0 flex-1">
          <p className="mb-6 text-xs font-medium text-muted-foreground">
            Messages per UTC day · last 14 days
          </p>
          {utcState === "loading" ? (
            <LoadingBlock label="Loading messages..." height={220} />
          ) : (
            <StackedBars
              data={data}
              series={[{ key: "requests", label: "Messages", color: "var(--src-app)" }]}
              height={220}
              format={fmt}
              unit="messages"
              ariaLabel="Messages per UTC day over the last 14 days"
              refLine={{ value: FREE_DAILY_MESSAGES, label: "Daily limit" }}
              maxBarWidth={22}
            />
          )}
          {utcState === "unavailable" && (
            <p className="mt-2 text-xs text-muted-foreground">
              Earlier days aren't available right now. Today's count is current.
            </p>
          )}
        </div>
      </div>
    </SettingsSection>
  );
}
