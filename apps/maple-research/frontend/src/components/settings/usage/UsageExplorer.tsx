import { useMemo, useState } from "react";
import type { UsageRange, UsageResponse } from "@/billing/usageApi";
import { SettingsSection } from "@/components/settings/SettingsPage";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectSeparator,
  SelectTrigger,
  SelectValue
} from "@/components/ui/select";
import { cn } from "@/utils/utils";
import { Legend, ShareBar, SplitBar, StackedBars, Swatch, type StackDatum } from "./charts";
import {
  modelColor,
  SOURCE_COLOR,
  SOURCE_LABEL,
  SOURCES,
  TOKEN_COLOR,
  TOKEN_KINDS,
  TOKEN_LABEL
} from "./palette";
import { ErrorBlock, LoadingBlock, SectionHeader, Segmented, StatTile } from "./ui";
import {
  buildDailyChart,
  dateRange,
  extraCreditSpan,
  fmt,
  fmtCompact,
  fmtDay,
  fmtPct,
  fmtWeekday,
  rangeLabel,
  rangeOptions,
  sortModels,
  summarizeDays,
  tokenTotal,
  type DayBucket,
  type Metric,
  type RangeSummary,
  type Split,
  type TokenTotals,
  type UsagePlanKind
} from "./usageData";

type UsageState = {
  data: UsageResponse | undefined;
  isError: boolean;
  isPlaceholderData: boolean;
  refetch: () => void;
};

function RangeSelect({
  value,
  onChange,
  today
}: {
  value: UsageRange;
  onChange: (value: UsageRange) => void;
  today: string;
}) {
  const { current, past } = rangeOptions(today);
  return (
    <Select value={value} onValueChange={(v) => onChange(v as UsageRange)}>
      <SelectTrigger className="h-9 w-[13.5rem] bg-card text-sm" aria-label="Usage range">
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        <SelectItem value="last_30_days">Last 30 days</SelectItem>
        <SelectItem value={current.value}>{current.label}</SelectItem>
        <SelectSeparator />
        <SelectGroup>
          <SelectLabel className="text-xs text-muted-foreground">Previous months</SelectLabel>
          {past.map((m) => (
            <SelectItem key={m.value} value={m.value}>
              {m.label}
            </SelectItem>
          ))}
        </SelectGroup>
      </SelectContent>
    </Select>
  );
}

function TokenBreakdown({
  tokens,
  cachedReported
}: {
  tokens: TokenTotals;
  cachedReported: number;
}) {
  const total = tokenTotal(tokens);
  return (
    <div>
      <div className="mt-3">
        <SplitBar
          parts={TOKEN_KINDS.map((k) => ({
            key: k,
            label: TOKEN_LABEL[k],
            value: tokens[k],
            color: TOKEN_COLOR[k]
          }))}
        />
      </div>
      <div className="mt-3 grid grid-cols-3 gap-3">
        {TOKEN_KINDS.map((k) => (
          <div key={k}>
            <p className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
              <Swatch color={TOKEN_COLOR[k]} /> {TOKEN_LABEL[k]}
            </p>
            <p className="text-base font-semibold tabular-nums">{fmtCompact(tokens[k])}</p>
            <p className="text-[11px] text-muted-foreground">
              {fmtPct(tokens[k], total)} of tokens
            </p>
          </div>
        ))}
      </div>
      {cachedReported > 0 && (
        <p className="mt-3 text-xs text-muted-foreground">
          {fmtPct(tokens.cached, tokens.input + tokens.cached)} of prompt tokens came from the
          cache.
        </p>
      )}
    </div>
  );
}

function StatTiles({
  summary,
  kind,
  label
}: {
  summary: RangeSummary;
  kind: UsagePlanKind;
  label: string;
}) {
  const tokens = fmtCompact(tokenTotal(summary.tokens));
  if (kind === "free") {
    return (
      <div className="grid gap-3 sm:grid-cols-3">
        <StatTile label="Messages" value={fmt(summary.requests)} sub={label} />
        <StatTile label="Tokens" value={tokens} className="sm:col-span-2">
          <TokenBreakdown tokens={summary.tokens} cachedReported={summary.cachedReported} />
        </StatTile>
      </div>
    );
  }
  const team = kind === "team";
  return (
    <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
      <StatTile label="Credits used" value={fmt(summary.credits)} sub={label} />
      <StatTile
        label="Requests"
        value={fmt(summary.requests)}
        sub={
          summary.requests > 0
            ? `About ${fmt(summary.credits / summary.requests)} credits each, background requests included`
            : "Background requests included"
        }
      />
      <StatTile
        label="Maple apps vs API keys"
        sub={`${fmtPct(summary.app, summary.credits)} apps · ${fmtPct(summary.api, summary.credits)} API`}
      >
        <div className="mt-3">
          <SplitBar
            parts={SOURCES.map((s) => ({
              key: s,
              label: SOURCE_LABEL[s],
              value: summary[s],
              color: SOURCE_COLOR[s]
            }))}
          />
        </div>
        <div className="mt-2.5 space-y-1 text-xs">
          {SOURCES.map((s) => (
            <div key={s} className="flex items-center justify-between gap-2">
              <span className="inline-flex items-center gap-1.5 text-foreground/80">
                <Swatch color={SOURCE_COLOR[s]} /> {SOURCE_LABEL[s]}
              </span>
              <span className="tabular-nums text-muted-foreground">{fmt(summary[s])}</span>
            </div>
          ))}
        </div>
      </StatTile>
      <StatTile
        label="Tokens"
        value={tokens}
        className={team ? "sm:col-span-2 lg:col-span-3" : "lg:col-span-2"}
      >
        <TokenBreakdown tokens={summary.tokens} cachedReported={summary.cachedReported} />
      </StatTile>
      {!team && (
        <StatTile
          label="Paid from Extra credits"
          value={fmt(summary.extra)}
          sub={
            summary.extra > 0
              ? "Plan credits ran out for part of this range"
              : "Plan credits covered everything"
          }
        />
      )}
    </div>
  );
}

function dayFooter(day: DayBucket, metric: Metric, free: boolean) {
  return (
    <span className="block space-y-0.5">
      {day.folded.length > 0 && (
        <span className="block">
          More models: {day.folded.map((f) => `${f.label} ${fmtCompact(f.value)}`).join(" · ")}
        </span>
      )}
      <span className="block">
        {fmt(day.requests)} {free ? "messages" : "requests"}
        {day.extraCredits > 0 && metric === "credits" && (
          <>
            {" · "}
            <span className="text-foreground">{fmt(day.extraCredits)} from Extra credits</span>
          </>
        )}
      </span>
    </span>
  );
}

const SPLIT_WORD: Record<Split, string> = {
  model: "model",
  source: "Maple apps and API keys",
  pool: "which balance paid",
  token: "input, cached input and output"
};

function DailyChart({
  usage,
  dates,
  kind,
  periodStartDate
}: {
  usage: UsageResponse;
  dates: string[];
  kind: UsagePlanKind;
  periodStartDate: string | null;
}) {
  const free = kind === "free";
  const [metric, setMetric] = useState<Metric>(free ? "requests" : "credits");
  const [split, setSplit] = useState<Split>("model");

  const chart = useMemo(
    () => buildDailyChart({ dates, rows: usage.days, metric, split, labels: usage.models }),
    [dates, usage, metric, split]
  );
  const data: StackDatum[] = chart.days.map((day) => ({
    key: day.date,
    tick: fmtDay(day.date),
    title: fmtWeekday(day.date),
    values: day.values,
    footer: dayFooter(day, metric, free)
  }));
  const markers =
    !free && periodStartDate && dates.includes(periodStartDate)
      ? [{ key: periodStartDate, label: "Plan credits reset" }]
      : [];
  const extraSpan = kind === "credits" && metric === "credits" ? extraCreditSpan(chart.days) : null;
  const ranges = extraSpan
    ? [
        {
          fromKey: extraSpan.from,
          toKey: extraSpan.to,
          label: "Plan credits ran out · Extra credits used"
        }
      ]
    : [];

  const changeMetric = (next: Metric) => {
    setMetric(next);
    if (next === "tokens") setSplit("token");
    else if (split === "token" || (split === "pool" && next !== "credits")) setSplit("model");
  };

  const metricOptions: Metric[] = free ? ["requests", "tokens"] : ["credits", "requests", "tokens"];
  const metricLabel = (m: Metric) =>
    m === "credits" ? "Credits" : m === "requests" ? (free ? "Messages" : "Requests") : "Tokens";
  const splitOptions: Array<{ value: Split; label: string }> = [
    { value: "model", label: "Model" },
    ...(free ? [] : [{ value: "source" as const, label: "Apps / API" }]),
    ...(kind === "credits" && metric === "credits"
      ? [{ value: "pool" as const, label: "Plan / Extra" }]
      : []),
    ...(metric === "tokens" ? [{ value: "token" as const, label: "Token type" }] : [])
  ];
  const unit = metricLabel(metric).toLowerCase();

  return (
    <SettingsSection>
      <SectionHeader
        title={`Daily ${unit}`}
        description={`Each bar is one day, split by ${SPLIT_WORD[split]}.`}
        right={
          <div className="flex flex-col items-start gap-2 sm:items-end">
            <div className="flex items-center gap-2 text-xs text-muted-foreground">
              Show
              <Segmented
                label="Show"
                value={metric}
                onChange={changeMetric}
                options={metricOptions.map((m) => ({ value: m, label: metricLabel(m) }))}
              />
            </div>
            {splitOptions.length > 1 && (
              <div className="flex items-center gap-2 text-xs text-muted-foreground">
                Split by
                <Segmented
                  label="Split by"
                  value={split}
                  onChange={setSplit}
                  options={splitOptions}
                />
              </div>
            )}
          </div>
        }
      />
      <StackedBars
        data={data}
        series={chart.series}
        height={250}
        format={fmtCompact}
        formatValue={fmt}
        unit={unit}
        ariaLabel={`Daily ${unit}, split by ${SPLIT_WORD[split]}`}
        markers={markers}
        ranges={ranges}
      />
      {chart.series.length > 0 ? (
        <div className="mt-4">
          <Legend items={chart.series} />
        </div>
      ) : (
        <p className="mt-4 text-xs text-muted-foreground">No usage in this range.</p>
      )}
    </SettingsSection>
  );
}

function ModelTable({ summary, free }: { summary: RangeSummary; free: boolean }) {
  const rows = sortModels(summary.byModel, free ? "requests" : "credits");
  const shareOf = (r: (typeof rows)[number]) => (free ? r.requests : r.credits);
  const max = Math.max(...rows.map(shareOf), 1);
  const whole = free ? summary.requests : summary.credits;
  return (
    <SettingsSection>
      <SectionHeader
        title="By model"
        description="Every model that ran for your account in this range, including background requests Maple makes for you."
      />
      {rows.length === 0 ? (
        <p className="text-sm text-muted-foreground">No usage in this range.</p>
      ) : (
        <div className="-mx-4 overflow-x-auto sm:-mx-6">
          <table className="w-full min-w-[46rem] text-sm">
            <thead>
              <tr className="border-b text-left text-xs text-muted-foreground">
                <th className="px-4 pb-2 font-medium sm:pl-6">Model</th>
                <th className="whitespace-nowrap px-3 pb-2 text-right font-medium">
                  {free ? "Messages" : "Requests"}
                </th>
                {TOKEN_KINDS.map((k) => (
                  <th key={k} className="whitespace-nowrap px-3 pb-2 text-right font-medium">
                    <span className="inline-flex items-center gap-1.5">
                      <Swatch color={TOKEN_COLOR[k]} className="h-2 w-2" />
                      {TOKEN_LABEL[k]}
                    </span>
                  </th>
                ))}
                {!free && (
                  <th className="whitespace-nowrap px-3 pb-2 text-right font-medium">Credits</th>
                )}
                <th className="w-[16%] px-4 pb-2 font-medium sm:pr-6">Share</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => {
                const color = modelColor(row.model);
                return (
                  <tr key={row.model} className="border-b border-border/50 last:border-0">
                    <td className="px-4 py-2.5 sm:pl-6">
                      <div className="flex items-center gap-2">
                        <Swatch color={color} />
                        <span className="whitespace-nowrap font-medium">{row.label}</span>
                      </div>
                      {!free && (
                        <p className="ml-[18px] mt-0.5 whitespace-nowrap text-[11px] text-muted-foreground">
                          {row.api === 0
                            ? SOURCE_LABEL.app
                            : row.app === 0
                              ? SOURCE_LABEL.api
                              : `${fmtPct(row.app, row.credits)} apps · ${fmtPct(row.api, row.credits)} API`}
                        </p>
                      )}
                    </td>
                    <td className="px-3 py-2.5 text-right tabular-nums text-muted-foreground">
                      {fmt(row.requests)}
                    </td>
                    {TOKEN_KINDS.map((k) => {
                      const unreported = k === "cached" && row.cachedReported === 0;
                      return (
                        <td
                          key={k}
                          className="whitespace-nowrap px-3 py-2.5 text-right tabular-nums text-muted-foreground"
                          title={unreported ? "This model doesn't report cached tokens" : undefined}
                        >
                          {unreported ? "—" : fmtCompact(row.tokens[k])}
                        </td>
                      );
                    })}
                    {!free && (
                      <td className="px-3 py-2.5 text-right font-medium tabular-nums">
                        {fmt(row.credits)}
                      </td>
                    )}
                    <td className="px-4 py-2.5 sm:pr-6">
                      <div className="flex items-center gap-2">
                        <div className="flex-1">
                          <ShareBar value={shareOf(row)} max={max} color={color} />
                        </div>
                        <span className="w-9 text-right text-xs tabular-nums text-muted-foreground">
                          {fmtPct(shareOf(row), whole)}
                        </span>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </SettingsSection>
  );
}

/** The dates the response covers, up to today (a current month runs to date). */
function responseDates(usage: UsageResponse, today: string): string[] {
  const { start_date: start, end_date: end } = usage.range;
  if (!start || !end) return [];
  const latestRow = usage.days.reduce((max, d) => (d.date > max ? d.date : max), today);
  return dateRange(start, end < latestRow ? end : latestRow);
}

export function UsageExplorer({
  kind,
  range,
  onRangeChange,
  usage,
  today,
  periodStartDate
}: {
  kind: UsagePlanKind;
  range: UsageRange;
  onRangeChange: (range: UsageRange) => void;
  usage: UsageState;
  today: string;
  periodStartDate: string | null;
}) {
  const label = rangeLabel(range, today);
  const data = usage.data;
  const summary = useMemo(() => (data ? summarizeDays(data.days, data.models) : null), [data]);
  const dates = useMemo(() => (data ? responseDates(data, today) : []), [data, today]);

  return (
    <>
      <div className="flex flex-wrap items-center justify-between gap-3 pt-2">
        <div>
          <h2 className="text-base font-semibold">Usage in this range</h2>
          <p className="mt-0.5 text-sm text-muted-foreground">{label}</p>
        </div>
        <RangeSelect value={range} onChange={onRangeChange} today={today} />
      </div>

      {data && summary ? (
        <div
          className={cn(
            "space-y-6 transition-opacity",
            usage.isPlaceholderData && "pointer-events-none opacity-60"
          )}
          aria-busy={usage.isPlaceholderData || undefined}
        >
          <StatTiles summary={summary} kind={kind} label={label} />
          <DailyChart usage={data} dates={dates} kind={kind} periodStartDate={periodStartDate} />
          <ModelTable summary={summary} free={kind === "free"} />
        </div>
      ) : usage.isError ? (
        <ErrorBlock message="Couldn't load usage for this range." onRetry={usage.refetch} />
      ) : (
        <SettingsSection>
          <LoadingBlock label="Loading usage..." height={220} />
        </SettingsSection>
      )}
    </>
  );
}
