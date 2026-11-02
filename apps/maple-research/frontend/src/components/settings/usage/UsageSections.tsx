import { useMemo, type ComponentType, type ReactNode } from "react";
import { Clock, Coins, Layers, ListTree, Loader2, Mic, Shuffle, Tag, Workflow } from "lucide-react";
import type { UsageModelLabels, UsageMonthRow } from "@/billing/usageApi";
import { useUsagePrices, useUsageRequests } from "@/billing/useUsage";
import { SettingsSection } from "@/components/settings/SettingsPage";
import { Button } from "@/components/ui/button";
import { Legend, StackedBars, Swatch, type StackDatum } from "./charts";
import {
  modelColor,
  SOURCE_COLOR,
  SOURCE_LABEL,
  SOURCES,
  TOKEN_KINDS,
  TOKEN_LABEL
} from "./palette";
import { ErrorBlock, LoadingBlock, SectionHeader } from "./ui";
import {
  buildMonths,
  fmt,
  fmtCompact,
  fmtMonth,
  fmtPrice,
  fmtRequestTime,
  FREE_DAILY_MESSAGES,
  modelLabel,
  type UsagePlanKind
} from "./usageData";

export function RecentRequests({
  accountId,
  kind,
  tz
}: {
  accountId: string;
  kind: UsagePlanKind;
  tz: string;
}) {
  const query = useUsageRequests(accountId);
  const free = kind === "free";
  const showPool = kind === "credits";
  const pages = query.data?.pages;
  const labels = useMemo(
    () => Object.assign({}, ...(pages ?? []).map((p) => p.models)) as UsageModelLabels,
    [pages]
  );
  const rows = useMemo(() => (pages ?? []).flatMap((p) => p.requests), [pages]);
  const now = new Date();

  return (
    <SettingsSection>
      <SectionHeader
        title="Recent requests"
        description="Every request your account made, newest first."
      />
      {query.isPending ? (
        <LoadingBlock label="Loading requests..." />
      ) : query.isError && rows.length === 0 ? (
        <ErrorBlock message="Couldn't load recent requests." onRetry={() => void query.refetch()} />
      ) : rows.length === 0 ? (
        <p className="text-sm text-muted-foreground">No requests yet.</p>
      ) : (
        <>
          <div className="-mx-4 overflow-x-auto sm:-mx-6">
            <table className="w-full min-w-[48rem] text-sm">
              <thead>
                <tr className="border-b text-left text-xs text-muted-foreground">
                  <th className="px-4 pb-2 font-medium sm:pl-6">Time</th>
                  <th className="px-3 pb-2 font-medium">Model</th>
                  {!free && <th className="px-3 pb-2 font-medium">Source</th>}
                  {TOKEN_KINDS.map((k) => (
                    <th key={k} className="whitespace-nowrap px-3 pb-2 text-right font-medium">
                      {TOKEN_LABEL[k]}
                    </th>
                  ))}
                  {!free && <th className="px-3 pb-2 text-right font-medium">Credits</th>}
                  {showPool && (
                    <th className="whitespace-nowrap px-4 pb-2 font-medium sm:pr-6">Paid from</th>
                  )}
                </tr>
              </thead>
              <tbody>
                {rows.map((r, i) => (
                  <tr key={`${r.time}-${i}`} className="border-b border-border/50 last:border-0">
                    <td className="whitespace-nowrap px-4 py-2 text-muted-foreground sm:pl-6">
                      {fmtRequestTime(r.time, now, tz)}
                    </td>
                    <td className="whitespace-nowrap px-3 py-2">
                      <span className="inline-flex items-center gap-2">
                        <Swatch color={modelColor(r.model)} />
                        {modelLabel(r.model, labels)}
                      </span>
                    </td>
                    {!free && (
                      <td className="whitespace-nowrap px-3 py-2">
                        <span className="inline-flex items-center gap-1.5">
                          <Swatch color={SOURCE_COLOR[r.source]} className="h-2 w-2 rounded-full" />
                          {r.source === "api" ? "API key" : "Maple app"}
                        </span>
                      </td>
                    )}
                    <td className="px-3 py-2 text-right tabular-nums text-muted-foreground">
                      {fmt(r.uncached_input_tokens)}
                    </td>
                    <td
                      className="px-3 py-2 text-right tabular-nums text-muted-foreground"
                      title={
                        r.cached_input_tokens === null
                          ? "This model didn't report cached tokens"
                          : undefined
                      }
                    >
                      {r.cached_input_tokens === null ? "—" : fmt(r.cached_input_tokens)}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums text-muted-foreground">
                      {fmt(r.output_tokens)}
                    </td>
                    {!free && (
                      <td className="px-3 py-2 text-right font-medium tabular-nums">
                        {fmt(r.credits)}
                      </td>
                    )}
                    {showPool && (
                      <td className="px-4 py-2 text-muted-foreground sm:pr-6">
                        {r.pool === "extra" ? "Extra" : "Plan"}
                      </td>
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {query.isError && (
            <div className="mt-3">
              <ErrorBlock
                message="Couldn't load more requests."
                onRetry={() => void query.fetchNextPage()}
              />
            </div>
          )}
          {query.hasNextPage && !query.isError && (
            <div className="mt-3 flex justify-center">
              <Button
                variant="ghost"
                size="sm"
                className="text-muted-foreground"
                disabled={query.isFetchingNextPage}
                onClick={() => void query.fetchNextPage()}
              >
                {query.isFetchingNextPage && <Loader2 className="mr-2 h-3.5 w-3.5 animate-spin" />}
                Load more
              </Button>
            </div>
          )}
        </>
      )}
    </SettingsSection>
  );
}

export function PricingTable({
  usedModels
}: {
  /** Models in the selected range; any without a current price shows "—". */
  usedModels: Array<{ model: string; label: string }>;
}) {
  const query = useUsagePrices();
  const prices = query.data?.models ?? [];
  const priced = new Set(prices.map((p) => p.id));
  const unpriced = usedModels.filter((m) => !priced.has(m.model));
  const varies = prices.some((p) => p.price_may_vary);

  return (
    <SettingsSection>
      <SectionHeader
        title="Current model prices"
        description="Credits per 1 million tokens, as charged today. 1,000 credits is $1 at list price."
      />
      {query.isPending ? (
        <LoadingBlock label="Loading prices..." />
      ) : query.isError ? (
        <ErrorBlock message="Couldn't load prices." onRetry={() => void query.refetch()} />
      ) : (
        <>
          <div className="-mx-4 overflow-x-auto sm:-mx-6">
            <table className="w-full min-w-[34rem] text-sm">
              <thead>
                <tr className="border-b text-left text-xs text-muted-foreground">
                  <th className="px-4 pb-2 font-medium sm:pl-6">Model</th>
                  {TOKEN_KINDS.map((k) => (
                    <th
                      key={k}
                      className="whitespace-nowrap px-3 pb-2 text-right font-medium last:pr-4 sm:last:pr-6"
                    >
                      {TOKEN_LABEL[k]}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {prices.map((p) => (
                  <tr key={p.id} className="border-b border-border/50 last:border-0">
                    <td className="whitespace-nowrap px-4 py-2 sm:pl-6">
                      {p.display_name || p.id}
                      {p.price_may_vary && <span className="text-muted-foreground">*</span>}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums">{fmtPrice(p.input)}</td>
                    <td className="px-3 py-2 text-right tabular-nums">
                      {p.cached_input === null ? (
                        <span className="text-muted-foreground">Same as input</span>
                      ) : (
                        fmtPrice(p.cached_input)
                      )}
                    </td>
                    <td className="px-3 py-2 pr-4 text-right tabular-nums sm:pr-6">
                      {fmtPrice(p.output)}
                    </td>
                  </tr>
                ))}
                {unpriced.map((m) => (
                  <tr key={m.model} className="border-b border-border/50 last:border-0">
                    <td className="whitespace-nowrap px-4 py-2 sm:pl-6">{m.label}</td>
                    {TOKEN_KINDS.map((k) => (
                      <td
                        key={k}
                        className="px-3 py-2 text-right text-muted-foreground last:pr-4 sm:last:pr-6"
                      >
                        —
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <p className="mt-3 text-xs leading-relaxed text-muted-foreground">
            Usage above shows the credits actually charged at the time, so older requests may not
            match today's prices.
            {unpriced.length > 0 && " Models marked — no longer have a listed price."}
            {varies && " * Depends on the provider that served the request; the default is shown."}
          </p>
        </>
      )}
    </SettingsSection>
  );
}

export function LastTwelveMonths({
  months,
  currentMonth
}: {
  months: UsageMonthRow[];
  currentMonth: string;
}) {
  const buckets = buildMonths(months, currentMonth);
  const series = SOURCES.map((s) => ({ key: s, label: SOURCE_LABEL[s], color: SOURCE_COLOR[s] }));
  const data: StackDatum[] = buckets.map((m) => ({
    key: m.month,
    tick: fmtMonth(m.month, "short"),
    title: m.partial ? `${fmtMonth(m.month)} (so far)` : fmtMonth(m.month),
    partial: m.partial,
    values: { app: m.app, api: m.api },
    footer: (
      <span>
        {fmt(m.requests)} requests
        {m.extraCredits > 0 && ` · ${fmt(m.extraCredits)} from Extra credits`}
      </span>
    )
  }));
  return (
    <SettingsSection>
      <SectionHeader
        title="Last 12 months"
        description="Credits per calendar month. The current month is shown so far."
      />
      <StackedBars
        data={data}
        series={series}
        height={200}
        format={fmtCompact}
        formatValue={fmt}
        unit="credits"
        ariaLabel="Credits per calendar month, Maple apps and API keys"
        maxBarWidth={34}
      />
      <div className="mt-4">
        <Legend items={series} />
      </div>
    </SettingsSection>
  );
}

export function AboutNumbers({
  kind,
  planName,
  allowance,
  seats,
  tz
}: {
  kind: UsagePlanKind;
  planName: string | null;
  allowance: number;
  seats: number | null;
  tz: string;
}) {
  const credits: ReactNode =
    kind === "team"
      ? seats && seats > 0
        ? `Your team shares ${fmt(allowance / seats)} credits per seat each billing period.`
        : `Your team shares ${fmt(allowance)} credits each billing period.`
      : `Your ${planName ? `${planName} plan` : "plan"} includes ${fmt(allowance)} credits each billing period. Extra credits are used only after plan credits run out, and they don't expire.`;

  const items: Array<{
    icon: ComponentType<{ className?: string }>;
    title: string;
    body: ReactNode;
  }> = [
    {
      icon: Layers,
      title: "Your whole account",
      body: "Maple Research and Maple Work on every device, plus your API keys. API keys include the Local Proxy in Maple Research and the Local API in Maple Work."
    },
    kind === "free"
      ? {
          icon: Coins,
          title: "Messages, not credits",
          body: `The Free plan allows ${FREE_DAILY_MESSAGES} messages per day, reset at midnight UTC. Paid plans use credits instead.`
        }
      : {
          icon: Coins,
          title: "Credits",
          body: `Credits measure usage at our API list prices: 1,000 credits is $1 of usage. ${credits}`
        },
    {
      icon: Workflow,
      title: "Background requests",
      body: "Maple also sends requests for you, for example to name chats, summarize tool results, choose a model for Auto, or shorten long conversations. They run on whichever model Maple uses for that job, which can change, and they count toward your usage. They are listed under that model."
    },
    {
      icon: Shuffle,
      title: "Auto, Quick and Powerful",
      body: "These choose a model for each request. Usage is listed under the model that actually ran."
    },
    {
      icon: ListTree,
      title: "Input, cached input and output",
      body: "Input is new prompt text. Cached input is prompt text the model reused from a recent request; models that support it charge less for it, and some models don't report it (shown as —). Output is what the model wrote. The three add up to your total tokens."
    },
    {
      icon: Tag,
      title: "Prices",
      body: "The price table shows today's prices. Usage shows the credits actually charged at the time."
    },
    {
      icon: Mic,
      title: "Not counted",
      body:
        kind === "free"
          ? "Voice input and read aloud don't count as messages."
          : "Voice input, read aloud and web search don't use credits."
    },
    {
      icon: Clock,
      title: "Time zone",
      body:
        kind === "free"
          ? `Days are shown in your time zone (${tz}). The daily message limit counts UTC days.`
          : `Days are shown in your time zone (${tz}).`
    }
  ];

  return (
    <SettingsSection title="About these numbers">
      <div className="grid gap-x-8 gap-y-5 sm:grid-cols-2">
        {items.map((item) => {
          const Icon = item.icon;
          return (
            <div key={item.title} className="flex items-start gap-3">
              <Icon className="mt-0.5 h-4 w-4 shrink-0 text-muted-foreground" />
              <div>
                <p className="text-sm font-medium">{item.title}</p>
                <p className="mt-0.5 text-sm leading-relaxed text-muted-foreground">{item.body}</p>
              </div>
            </div>
          );
        })}
      </div>
    </SettingsSection>
  );
}
