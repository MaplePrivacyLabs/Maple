import type { BillingStatus } from "@/billing/billingApi";
import { hasApiAccess, isKnownFreePlan } from "@/billing/billingAccess";
import type {
  UsageCounts,
  UsageCurrentPeriod,
  UsageDayRow,
  UsageModelLabels,
  UsageMonthRow,
  UsageRange
} from "@/billing/usageApi";
import {
  MODEL_SLOTS,
  MORE_MODELS_COLOR,
  MORE_MODELS_KEY,
  POOL_COLOR,
  POOL_LABEL,
  POOLS,
  SOURCE_COLOR,
  SOURCE_LABEL,
  SOURCES,
  TOKEN_COLOR,
  TOKEN_KINDS,
  TOKEN_LABEL,
  type TokenKind
} from "./palette";

// Pure transforms from billing usage responses to what the Usage page draws.
// Calendar dates stay YYYY-MM-DD strings; arithmetic on them runs in UTC so a
// DST change in the viewer's zone never skips or repeats a day.

const DAY_MS = 86_400_000;
/** Longest date list we build (yearly billing windows plus slack). */
const MAX_DAYS = 400;

export type Metric = "credits" | "requests" | "tokens";
export type Split = "model" | "source" | "pool" | "token";

/** Billing's Free plan limit: requests per UTC day (not exposed by status). */
export const FREE_DAILY_MESSAGES = 100;

// ---------------------------------------------------------------------------
// Dates

function dateMs(date: string): number {
  const [y, m, d] = date.split("-").map(Number);
  return Date.UTC(y, (m ?? 1) - 1, d ?? 1);
}

function isoDate(ms: number): string {
  return new Date(ms).toISOString().slice(0, 10);
}

export function addDays(date: string, days: number): string {
  return isoDate(dateMs(date) + days * DAY_MS);
}

export function daysBetween(from: string, to: string): number {
  return Math.round((dateMs(to) - dateMs(from)) / DAY_MS);
}

/** Inclusive list of dates from `start` to `end`. */
export function dateRange(start: string, end: string): string[] {
  const count = Math.min(MAX_DAYS, daysBetween(start, end) + 1);
  const out: string[] = [];
  for (let i = 0; i < count; i += 1) out.push(addDays(start, i));
  return out;
}

const dateFormatters = new Map<string, Intl.DateTimeFormat>();

function dateFormatter(tz: string): Intl.DateTimeFormat {
  let formatter = dateFormatters.get(tz);
  if (!formatter) {
    try {
      formatter = new Intl.DateTimeFormat("en-US", {
        timeZone: tz,
        year: "numeric",
        month: "2-digit",
        day: "2-digit"
      });
    } catch {
      formatter = dateFormatter("UTC");
    }
    dateFormatters.set(tz, formatter);
  }
  return formatter;
}

/** Calendar date (YYYY-MM-DD) of an instant in an IANA zone. */
export function localDate(at: Date, tz: string): string {
  const parts = dateFormatter(tz).formatToParts(at);
  const get = (type: string) => parts.find((p) => p.type === type)?.value ?? "01";
  return `${get("year")}-${get("month")}-${get("day")}`;
}

export function browserTimeZone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
  } catch {
    return "UTC";
  }
}

// ---------------------------------------------------------------------------
// Formatting

const intFormatter = new Intl.NumberFormat("en-US");
const compactFormatter = new Intl.NumberFormat("en-US", {
  notation: "compact",
  maximumFractionDigits: 1
});
const priceFormatter = new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 });

export function fmt(n: number): string {
  return intFormatter.format(Math.round(n));
}

export function fmtCompact(n: number): string {
  return compactFormatter.format(n);
}

export function fmtPrice(n: number): string {
  return priceFormatter.format(n);
}

export function fmtPct(part: number, whole: number): string {
  if (whole <= 0) return "0%";
  return `${Math.round((part / whole) * 100)}%`;
}

function utcDate(date: string): Date {
  return new Date(dateMs(date));
}

/** "Oct 14" */
export function fmtDay(date: string): string {
  return utcDate(date).toLocaleDateString("en-US", {
    month: "short",
    day: "numeric",
    timeZone: "UTC"
  });
}

/** "Wed, Oct 14" */
export function fmtWeekday(date: string): string {
  return utcDate(date).toLocaleDateString("en-US", {
    weekday: "short",
    month: "short",
    day: "numeric",
    timeZone: "UTC"
  });
}

/** "Oct 14 – Nov 13", or with years when the range crosses a year. */
export function fmtRange(start: string, end: string): string {
  if (start.slice(0, 4) === end.slice(0, 4)) return `${fmtDay(start)} – ${fmtDay(end)}`;
  const withYear = (d: string) =>
    utcDate(d).toLocaleDateString("en-US", {
      month: "short",
      day: "numeric",
      year: "numeric",
      timeZone: "UTC"
    });
  return `${withYear(start)} – ${withYear(end)}`;
}

/** "October 2026" or "Oct" for a YYYY-MM month. */
export function fmtMonth(month: string, style: "long" | "short" = "long"): string {
  return utcDate(`${month}-01`).toLocaleDateString("en-US", {
    month: style,
    ...(style === "long" ? { year: "numeric" } : {}),
    timeZone: "UTC"
  });
}

function fmtClock(at: Date, tz: string): string {
  return at.toLocaleTimeString("en-US", { hour: "numeric", minute: "2-digit", timeZone: tz });
}

/** "Today 2:41 PM", "Yesterday 9:10 AM", "Oct 26, 2:41 PM" in the viewer's zone. */
export function fmtRequestTime(iso: string, now: Date, tz: string): string {
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return "—";
  const day = localDate(at, tz);
  const today = localDate(now, tz);
  const clock = fmtClock(at, tz);
  if (day === today) return `Today ${clock}`;
  if (day === addDays(today, -1)) return `Yesterday ${clock}`;
  const sameYear = day.slice(0, 4) === today.slice(0, 4);
  const date = utcDate(day).toLocaleDateString("en-US", {
    month: "short",
    day: "numeric",
    ...(sameYear ? {} : { year: "numeric" }),
    timeZone: "UTC"
  });
  return `${date}, ${clock}`;
}

// ---------------------------------------------------------------------------
// Ranges

export type RangeOption = { value: UsageRange; label: string };

/** "This month" plus the 12 previous calendar months, newest first. */
export function rangeOptions(today: string): { current: RangeOption; past: RangeOption[] } {
  const month = today.slice(0, 7);
  const first = `${month}-01`;
  const currentLabel =
    first === today ? fmtDay(today) : `${fmtDay(first)} – ${Number(today.slice(8, 10))}`;
  const past: RangeOption[] = [];
  for (let i = 1; i <= 12; i += 1) {
    const m = shiftMonth(month, -i);
    past.push({ value: `month:${m}`, label: fmtMonth(m) });
  }
  return { current: { value: `month:${month}`, label: `This month · ${currentLabel}` }, past };
}

export function shiftMonth(month: string, delta: number): string {
  const [y, m] = month.split("-").map(Number);
  const index = y * 12 + (m - 1) + delta;
  return `${Math.floor(index / 12)}-${String((index % 12) + 1).padStart(2, "0")}`;
}

function lastDayOfMonth(month: string): string {
  return addDays(`${shiftMonth(month, 1)}-01`, -1);
}

/** The dates a range covers, cut at today (a current month runs to date). */
export function rangeDates(range: UsageRange, today: string): string[] {
  if (range === "last_30_days") return dateRange(addDays(today, -29), today);
  const month = range.slice("month:".length);
  const last = lastDayOfMonth(month);
  return dateRange(`${month}-01`, last < today ? last : today);
}

export function rangeLabel(range: UsageRange, today: string): string {
  const dates = rangeDates(range, today);
  if (dates.length === 0) return "";
  const start = dates[0];
  const end = dates[dates.length - 1];
  if (range === "last_30_days") return fmtRange(start, end);
  const month = range.slice("month:".length);
  if (month === today.slice(0, 7)) return `${fmtRange(start, end)}, month to date`;
  return fmtMonth(month);
}

// ---------------------------------------------------------------------------
// Totals

export type TokenTotals = Record<TokenKind, number>;

export type ModelSummary = {
  model: string;
  label: string;
  credits: number;
  requests: number;
  app: number;
  api: number;
  tokens: TokenTotals;
  /** Requests that reported a cached count; 0 means show "—", not 0. */
  cachedReported: number;
};

export type RangeSummary = {
  credits: number;
  requests: number;
  /** Credits by source. */
  app: number;
  api: number;
  /** Credits by pool. */
  plan: number;
  extra: number;
  tokens: TokenTotals;
  cachedReported: number;
  byModel: ModelSummary[];
};

export function modelLabel(model: string, labels: UsageModelLabels | undefined): string {
  return labels?.[model]?.display_name || model;
}

export function tokensOf(row: UsageCounts): TokenTotals {
  return {
    input: row.uncached_input_tokens,
    cached: row.cached_input_tokens,
    output: row.output_tokens
  };
}

export function tokenTotal(t: TokenTotals): number {
  return t.input + t.cached + t.output;
}

export function metricOf(row: UsageCounts, metric: Metric): number {
  if (metric === "credits") return row.credits;
  if (metric === "requests") return row.requests;
  return tokenTotal(tokensOf(row));
}

function emptyTokens(): TokenTotals {
  return { input: 0, cached: 0, output: 0 };
}

function addTokens(into: TokenTotals, row: UsageCounts) {
  into.input += row.uncached_input_tokens;
  into.cached += row.cached_input_tokens;
  into.output += row.output_tokens;
}

/** Totals for the stat tiles and the per-model table (credits first). */
export function summarizeDays(rows: UsageDayRow[], labels: UsageModelLabels): RangeSummary {
  const summary: RangeSummary = {
    credits: 0,
    requests: 0,
    app: 0,
    api: 0,
    plan: 0,
    extra: 0,
    tokens: emptyTokens(),
    cachedReported: 0,
    byModel: []
  };
  const byModel = new Map<string, ModelSummary>();
  for (const row of rows) {
    summary.credits += row.credits;
    summary.requests += row.requests;
    summary[row.source === "api" ? "api" : "app"] += row.credits;
    summary[row.pool === "extra" ? "extra" : "plan"] += row.credits;
    summary.cachedReported += row.cached_reported_requests;
    addTokens(summary.tokens, row);

    let model = byModel.get(row.model);
    if (!model) {
      model = {
        model: row.model,
        label: modelLabel(row.model, labels),
        credits: 0,
        requests: 0,
        app: 0,
        api: 0,
        tokens: emptyTokens(),
        cachedReported: 0
      };
      byModel.set(row.model, model);
    }
    model.credits += row.credits;
    model.requests += row.requests;
    model[row.source === "api" ? "api" : "app"] += row.credits;
    model.cachedReported += row.cached_reported_requests;
    addTokens(model.tokens, row);
  }
  summary.byModel = sortModels([...byModel.values()], "credits");
  return summary;
}

export function sortModels(models: ModelSummary[], by: "credits" | "requests"): ModelSummary[] {
  const other = by === "credits" ? "requests" : "credits";
  return [...models].sort(
    (a, b) => b[by] - a[by] || b[other] - a[other] || a.label.localeCompare(b.label)
  );
}

// ---------------------------------------------------------------------------
// Daily chart

export type SeriesDef = { key: string; label: string; color: string };

export type DayBucket = {
  date: string;
  values: Record<string, number>;
  requests: number;
  extraCredits: number;
  /** Models behind "More models" this day, largest first. */
  folded: Array<{ model: string; label: string; value: number }>;
};

export type DailyChart = { series: SeriesDef[]; days: DayBucket[] };

const SLOT_ORDER = Object.keys(MODEL_SLOTS);

function seriesKey(row: UsageDayRow, split: Split): string {
  if (split === "source") return row.source;
  if (split === "pool") return row.pool;
  return row.model in MODEL_SLOTS ? row.model : MORE_MODELS_KEY;
}

/**
 * Pivot billing's day × model × source × pool rows into stacked series for a
 * metric and split. Only series with usage in the range are returned, in a
 * fixed identity order, so colors and stacking never depend on rank.
 */
export function buildDailyChart({
  dates,
  rows,
  metric,
  split,
  labels
}: {
  dates: string[];
  rows: UsageDayRow[];
  metric: Metric;
  split: Split;
  labels: UsageModelLabels;
}): DailyChart {
  const buckets = new Map<string, DayBucket>(
    dates.map((date) => [date, { date, values: {}, requests: 0, extraCredits: 0, folded: [] }])
  );
  const totals = new Map<string, number>();
  const foldedModels = new Set<string>();

  for (const row of rows) {
    const day = buckets.get(row.date);
    if (!day) continue;
    day.requests += row.requests;
    if (row.pool === "extra") day.extraCredits += row.credits;

    if (split === "token") {
      const tokens = tokensOf(row);
      for (const kind of TOKEN_KINDS) {
        day.values[kind] = (day.values[kind] ?? 0) + tokens[kind];
        totals.set(kind, (totals.get(kind) ?? 0) + tokens[kind]);
      }
      continue;
    }

    const value = metricOf(row, metric);
    const key = seriesKey(row, split);
    day.values[key] = (day.values[key] ?? 0) + value;
    totals.set(key, (totals.get(key) ?? 0) + value);

    if (key === MORE_MODELS_KEY && value > 0) {
      foldedModels.add(row.model);
      const existing = day.folded.find((f) => f.model === row.model);
      if (existing) existing.value += value;
      else day.folded.push({ model: row.model, label: modelLabel(row.model, labels), value });
    }
  }

  const used = (key: string) => (totals.get(key) ?? 0) > 0;
  let series: SeriesDef[];
  if (split === "token") {
    series = TOKEN_KINDS.filter(used).map((k) => ({
      key: k,
      label: TOKEN_LABEL[k],
      color: TOKEN_COLOR[k]
    }));
  } else if (split === "source") {
    series = SOURCES.filter(used).map((s) => ({
      key: s,
      label: SOURCE_LABEL[s],
      color: SOURCE_COLOR[s]
    }));
  } else if (split === "pool") {
    series = POOLS.filter(used).map((p) => ({
      key: p,
      label: POOL_LABEL[p],
      color: POOL_COLOR[p]
    }));
  } else {
    series = SLOT_ORDER.filter(used).map((m) => ({
      key: m,
      label: modelLabel(m, labels),
      color: MODEL_SLOTS[m]
    }));
    if (used(MORE_MODELS_KEY)) {
      // One unslotted model is named outright; only a real fold says "More models".
      const only = foldedModels.size === 1 ? [...foldedModels][0] : null;
      series.push({
        key: MORE_MODELS_KEY,
        label: only ? modelLabel(only, labels) : "More models",
        color: MORE_MODELS_COLOR
      });
    }
  }

  const days = [...buckets.values()];
  for (const day of days) {
    day.folded =
      foldedModels.size > 1
        ? day.folded.sort((a, b) => b.value - a.value)
        : ([] as DayBucket["folded"]);
  }
  return { series, days };
}

/** First and last day in the range that drew on Extra credits. */
export function extraCreditSpan(days: DayBucket[]): { from: string; to: string } | null {
  const extra = days.filter((d) => d.extraCredits > 0);
  if (extra.length === 0) return null;
  return { from: extra[0].date, to: extra[extra.length - 1].date };
}

// ---------------------------------------------------------------------------
// Last 12 months

export type MonthBucket = {
  month: string;
  app: number;
  api: number;
  requests: number;
  extraCredits: number;
  partial: boolean;
};

/** Credits per calendar month for the `count` months ending at `currentMonth`. */
export function buildMonths(
  rows: UsageMonthRow[],
  currentMonth: string,
  count = 12
): MonthBucket[] {
  const buckets = new Map<string, MonthBucket>();
  for (let i = count - 1; i >= 0; i -= 1) {
    const month = shiftMonth(currentMonth, -i);
    buckets.set(month, {
      month,
      app: 0,
      api: 0,
      requests: 0,
      extraCredits: 0,
      partial: month === currentMonth
    });
  }
  for (const row of rows) {
    const bucket = buckets.get(row.month);
    if (!bucket) continue;
    bucket[row.source === "api" ? "api" : "app"] += row.credits;
    bucket.requests += row.requests;
    if (row.pool === "extra") bucket.extraCredits += row.credits;
  }
  return [...buckets.values()];
}

// ---------------------------------------------------------------------------
// Plan period

export type UsagePlanKind = "free" | "team" | "credits";

/** Which plan card the page shows. */
export function usagePlanKind(status: BillingStatus | null | undefined): UsagePlanKind {
  const name = status?.product_name?.toLowerCase() ?? "";
  if (isKnownFreePlan(status)) return "free";
  if (name.includes("team")) return "team";
  if ((status?.total_tokens ?? 0) > 0 || hasApiAccess(status)) return "credits";
  return "free";
}

/** The status usage window, for `/usage`'s `period_start`/`period_end`. */
export function statusWindow(
  status: BillingStatus | null | undefined
): { start: string; end: string } | null {
  if (!status || usagePlanKind(status) === "free") return null;
  const start = status.usage_period_start;
  const end = status.usage_reset_date;
  if (!start || !end) return null;
  const startMs = new Date(start).getTime();
  const endMs = new Date(end).getTime();
  if (!Number.isFinite(startMs) || !Number.isFinite(endMs) || endMs <= startMs) return null;
  return { start, end };
}

export type PeriodProgress = {
  /** Local dates of the window, one per chart index. */
  dates: string[];
  todayIndex: number;
  /** Cumulative plan credits per day through today. */
  actual: number[];
  used: number;
  allowance: number;
  /** Average credits per elapsed day. */
  pace: number;
  /** Plan credits at the window's end at the current pace. */
  projected: number;
  /** Local date the pace reaches the allowance, if inside the window. */
  runOutDate: string | null;
  exhausted: boolean;
  daysLeft: number;
  ticks: number[];
};

/** Tick indexes: weekly for a month-long window, five even steps otherwise. */
export function periodTicks(totalDays: number): number[] {
  if (totalDays <= 1) return [0];
  const last = totalDays - 1;
  if (totalDays <= 45) {
    const ticks: number[] = [];
    for (let i = 0; i < last - 3; i += 7) ticks.push(i);
    ticks.push(last);
    return ticks;
  }
  return [0, 1, 2, 3, 4].map((k) => Math.round((k * last) / 4));
}

/**
 * Cumulative plan use over the billing window with a straight-line projection.
 * `used` comes from status (the meter); the chart's last point is pinned to it
 * so the line and the meter agree even when the daily series lags.
 */
export function buildPeriodProgress({
  window,
  days,
  used,
  allowance,
  tz,
  now
}: {
  window: { start: string; end: string };
  days: UsageCurrentPeriod["days"];
  used: number;
  allowance: number;
  tz: string;
  now: Date;
}): PeriodProgress | null {
  const startMs = new Date(window.start).getTime();
  const endMs = new Date(window.end).getTime();
  if (!Number.isFinite(startMs) || !Number.isFinite(endMs) || endMs <= startMs) return null;

  const dates = dateRange(localDate(new Date(startMs), tz), localDate(new Date(endMs - 1), tz));
  if (dates.length === 0) return null;
  const totalDays = dates.length;
  const today = localDate(now, tz);
  const todayIndex = Math.min(totalDays - 1, Math.max(0, daysBetween(dates[0], today)));

  const byDate = new Map<string, number>();
  for (const day of days) byDate.set(day.date, (byDate.get(day.date) ?? 0) + day.plan_credits);
  const actual: number[] = [];
  let sum = 0;
  for (let i = 0; i <= todayIndex; i += 1) {
    sum += byDate.get(dates[i]) ?? 0;
    actual.push(Math.min(sum, used));
  }
  actual[todayIndex] = used;

  const elapsed = todayIndex + 1;
  const pace = used / elapsed;
  const projected = used + pace * (totalDays - elapsed);
  const exhausted = used >= allowance;
  let runOutDate: string | null = null;
  if (!exhausted && pace > 0) {
    const index = todayIndex + Math.ceil((allowance - used) / pace);
    runOutDate = index < totalDays ? dates[index] : null;
  }

  return {
    dates,
    todayIndex,
    actual,
    used,
    allowance,
    pace,
    projected,
    runOutDate,
    exhausted,
    daysLeft: Math.max(0, Math.ceil((endMs - now.getTime()) / DAY_MS)),
    ticks: periodTicks(totalDays)
  };
}

// ---------------------------------------------------------------------------
// Free plan

export type FreeDay = { date: string; requests: number; partial: boolean };

/**
 * Messages per UTC day for the last `count` days. Rows must come from a
 * `tz=UTC` request because the Free limit resets at 00:00 UTC. Today's bar
 * uses the status count when given, which is live.
 */
export function buildFreeDays({
  rows,
  now,
  count = 14,
  usedToday
}: {
  rows: UsageDayRow[];
  now: Date;
  count?: number;
  usedToday?: number;
}): FreeDay[] {
  const today = now.toISOString().slice(0, 10);
  const byDate = new Map<string, number>();
  for (const row of rows) byDate.set(row.date, (byDate.get(row.date) ?? 0) + row.requests);
  return dateRange(addDays(today, -(count - 1)), today).map((date) => ({
    date,
    requests: date === today && usedToday !== undefined ? usedToday : (byDate.get(date) ?? 0),
    partial: date === today
  }));
}

export function freeUsedToday(chatsRemaining: number | null | undefined): number {
  if (chatsRemaining === null || chatsRemaining === undefined) return 0;
  return Math.min(FREE_DAILY_MESSAGES, Math.max(0, FREE_DAILY_MESSAGES - chatsRemaining));
}

export function nextUtcMidnight(now: Date): Date {
  return new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() + 1));
}

/** "Resets at 7:00 PM your time (midnight UTC)." */
export function freeResetLabel(now: Date, tz: string): string {
  const reset = nextUtcMidnight(now);
  if (fmtClock(reset, tz) === fmtClock(reset, "UTC")) return "Resets at midnight UTC.";
  return `Resets at ${fmtClock(reset, tz)} your time (midnight UTC).`;
}
