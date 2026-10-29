import { describe, expect, test } from "bun:test";
import type { BillingStatus } from "@/billing/billingApi";
import type { UsageDayRow, UsageMonthRow } from "@/billing/usageApi";
import { niceScale } from "./chartScale";
import { MORE_MODELS_KEY } from "./palette";
import {
  addDays,
  buildDailyChart,
  buildFreeDays,
  buildMonths,
  buildPeriodProgress,
  dateRange,
  extraCreditSpan,
  fmtRange,
  fmtRequestTime,
  freeResetLabel,
  freeUsedToday,
  localDate,
  nextUtcMidnight,
  periodTicks,
  rangeDates,
  rangeLabel,
  rangeOptions,
  shiftMonth,
  statusWindow,
  summarizeDays,
  usagePlanKind
} from "./usageData";

function row(partial: Partial<UsageDayRow> & Pick<UsageDayRow, "date" | "model">): UsageDayRow {
  return {
    source: "app",
    pool: "plan",
    requests: 1,
    uncached_input_tokens: 0,
    cached_input_tokens: 0,
    cached_reported_requests: 0,
    output_tokens: 0,
    credits: 0,
    ...partial
  };
}

const labels = {
  "glm-5-3": { display_name: "GLM-5.3" },
  "kimi-k3": { display_name: "Kimi K3" },
  "llama3-3-70b": { display_name: "Llama 3.3 70B" },
  "gemma4-31b": { display_name: "Gemma 4 31B" },
  "kimi-k2-6": { display_name: null }
};

const status = (overrides: Partial<BillingStatus>): BillingStatus => ({
  is_subscribed: true,
  product_id: "prod",
  product_name: "Pro",
  subscription_status: "active",
  current_period_end: null,
  can_chat: true,
  chats_remaining: null,
  payment_provider: "stripe",
  total_tokens: 20_000,
  used_tokens: 1_000,
  usage_reset_date: "2026-11-14T05:09:23Z",
  usage_period_start: "2026-10-14T05:09:23Z",
  ...overrides
});

describe("dates", () => {
  test("date arithmetic is calendar based across DST and month ends", () => {
    expect(addDays("2026-10-31", 1)).toBe("2026-11-01");
    expect(addDays("2026-03-08", 1)).toBe("2026-03-09");
    expect(dateRange("2026-02-27", "2026-03-02")).toEqual([
      "2026-02-27",
      "2026-02-28",
      "2026-03-01",
      "2026-03-02"
    ]);
    expect(dateRange("2026-03-02", "2026-03-01")).toEqual([]);
  });

  test("localDate reads the calendar date in the given zone", () => {
    const at = new Date("2026-10-29T03:30:00Z");
    expect(localDate(at, "UTC")).toBe("2026-10-29");
    expect(localDate(at, "America/Chicago")).toBe("2026-10-28");
    expect(localDate(at, "Asia/Kolkata")).toBe("2026-10-29");
    expect(localDate(at, "Not/AZone")).toBe("2026-10-29");
  });

  test("shiftMonth crosses year boundaries", () => {
    expect(shiftMonth("2026-01", -1)).toBe("2025-12");
    expect(shiftMonth("2025-12", 1)).toBe("2026-01");
    expect(shiftMonth("2026-10", -12)).toBe("2025-10");
  });

  test("fmtRange adds years only when the range crosses one", () => {
    expect(fmtRange("2026-10-14", "2026-11-14")).toBe("Oct 14 – Nov 14");
    expect(fmtRange("2026-12-20", "2027-01-20")).toBe("Dec 20, 2026 – Jan 20, 2027");
  });

  test("request times read as today, yesterday or a date in the viewer's zone", () => {
    const now = new Date("2026-10-28T20:00:00Z");
    expect(fmtRequestTime("2026-10-28T19:41:07.123Z", now, "America/Chicago")).toBe(
      "Today 2:41 PM"
    );
    expect(fmtRequestTime("2026-10-28T03:00:00Z", now, "America/Chicago")).toBe(
      "Yesterday 10:00 PM"
    );
    expect(fmtRequestTime("2026-10-20T15:05:00Z", now, "UTC")).toBe("Oct 20, 3:05 PM");
    expect(fmtRequestTime("2025-12-31T15:05:00Z", now, "UTC")).toBe("Dec 31, 2025, 3:05 PM");
    expect(fmtRequestTime("not a date", now, "UTC")).toBe("—");
  });
});

describe("ranges", () => {
  test("last 30 days ends today and a current month runs to date", () => {
    const last30 = rangeDates("last_30_days", "2026-10-28");
    expect(last30).toHaveLength(30);
    expect(last30[0]).toBe("2026-09-29");
    expect(last30[29]).toBe("2026-10-28");
    expect(rangeDates("month:2026-10", "2026-10-28")).toHaveLength(28);
    expect(rangeDates("month:2026-02", "2026-10-28")).toHaveLength(28);
  });

  test("labels name the range plainly", () => {
    expect(rangeLabel("last_30_days", "2026-10-28")).toBe("Sep 29 – Oct 28");
    expect(rangeLabel("month:2026-10", "2026-10-28")).toBe("Oct 1 – Oct 28, month to date");
    expect(rangeLabel("month:2026-09", "2026-10-28")).toBe("September 2026");
  });

  test("the picker offers this month and the 12 before it", () => {
    const { current, past } = rangeOptions("2026-10-28");
    expect(current).toEqual({ value: "month:2026-10", label: "This month · Oct 1 – 28" });
    expect(past).toHaveLength(12);
    expect(past[0]).toEqual({ value: "month:2026-09", label: "September 2026" });
    expect(past[11].value).toBe("month:2025-10");
    expect(rangeOptions("2026-10-01").current.label).toBe("This month · Oct 1");
  });
});

describe("summarizeDays", () => {
  test("totals split by source and pool and keep tokens apart", () => {
    const summary = summarizeDays(
      [
        row({
          date: "2026-10-01",
          model: "glm-5-3",
          source: "app",
          credits: 54,
          requests: 12,
          uncached_input_tokens: 1000,
          cached_input_tokens: 3000,
          cached_reported_requests: 12,
          output_tokens: 200
        }),
        row({
          date: "2026-10-02",
          model: "glm-5-3",
          source: "api",
          pool: "extra",
          credits: 6,
          requests: 1,
          uncached_input_tokens: 10,
          output_tokens: 5,
          cached_reported_requests: 1
        }),
        row({
          date: "2026-10-02",
          model: "llama3-3-70b",
          credits: 1,
          requests: 3,
          uncached_input_tokens: 400,
          output_tokens: 9
        })
      ],
      labels
    );
    expect(summary).toMatchObject({
      credits: 61,
      requests: 16,
      app: 55,
      api: 6,
      plan: 55,
      extra: 6,
      tokens: { input: 1410, cached: 3000, output: 214 },
      cachedReported: 13
    });
    expect(summary.byModel.map((m) => [m.model, m.label, m.credits, m.app, m.api])).toEqual([
      ["glm-5-3", "GLM-5.3", 60, 54, 6],
      ["llama3-3-70b", "Llama 3.3 70B", 1, 1, 0]
    ]);
    // A model that never reported cached tokens shows "—", not 0.
    expect(summary.byModel[1].cachedReported).toBe(0);
  });

  test("a model without a label falls back to its id", () => {
    const summary = summarizeDays([row({ date: "2026-10-01", model: "kimi-k2-6" })], labels);
    expect(summary.byModel[0].label).toBe("kimi-k2-6");
  });
});

describe("buildDailyChart", () => {
  const dates = ["2026-10-01", "2026-10-02", "2026-10-03"];
  const rows = [
    row({ date: "2026-10-01", model: "kimi-k3", credits: 30, requests: 2 }),
    row({ date: "2026-10-01", model: "glm-5-3", credits: 50, requests: 4 }),
    row({ date: "2026-10-01", model: "llama3-3-70b", credits: 2, requests: 5 }),
    row({ date: "2026-10-02", model: "gemma4-31b", credits: 7, requests: 1, source: "api" }),
    row({
      date: "2026-10-02",
      model: "glm-5-3",
      credits: 20,
      requests: 1,
      pool: "extra",
      source: "api",
      uncached_input_tokens: 100,
      cached_input_tokens: 50,
      output_tokens: 25
    }),
    // Outside the chart's dates; ignored.
    row({ date: "2026-09-30", model: "glm-5-3", credits: 999 })
  ];

  test("model split uses identity order and folds unslotted models", () => {
    const chart = buildDailyChart({ dates, rows, metric: "credits", split: "model", labels });
    expect(chart.series.map((s) => [s.key, s.label, s.color])).toEqual([
      ["glm-5-3", "GLM-5.3", "var(--series-1)"],
      ["kimi-k3", "Kimi K3", "var(--series-2)"],
      [MORE_MODELS_KEY, "More models", "var(--series-other)"]
    ]);
    expect(chart.days.map((d) => d.date)).toEqual(dates);
    expect(chart.days[0].values).toEqual({ "kimi-k3": 30, "glm-5-3": 50, more: 2 });
    expect(chart.days[1].values).toEqual({ more: 7, "glm-5-3": 20 });
    expect(chart.days[2].values).toEqual({});
    expect(chart.days[0].folded).toEqual([
      { model: "llama3-3-70b", label: "Llama 3.3 70B", value: 2 }
    ]);
    expect(chart.days[0].requests).toBe(11);
    expect(chart.days[1].extraCredits).toBe(20);
  });

  test("a single unslotted model is named instead of folded", () => {
    const chart = buildDailyChart({
      dates,
      rows: rows.filter((r) => r.model !== "gemma4-31b"),
      metric: "requests",
      split: "model",
      labels
    });
    expect(chart.series.at(-1)).toEqual({
      key: MORE_MODELS_KEY,
      label: "Llama 3.3 70B",
      color: "var(--series-other)"
    });
    expect(chart.days[0].values.more).toBe(5);
    expect(chart.days[0].folded).toEqual([]);
  });

  test("source, pool and token splits pivot the same rows", () => {
    const source = buildDailyChart({ dates, rows, metric: "credits", split: "source", labels });
    expect(source.series.map((s) => s.key)).toEqual(["app", "api"]);
    expect(source.days[1].values).toEqual({ api: 27 });

    const pool = buildDailyChart({ dates, rows, metric: "credits", split: "pool", labels });
    expect(pool.series.map((s) => s.label)).toEqual(["Plan credits", "Extra credits"]);
    expect(pool.days[1].values).toEqual({ plan: 7, extra: 20 });

    const tokens = buildDailyChart({ dates, rows, metric: "tokens", split: "token", labels });
    expect(tokens.series.map((s) => s.key)).toEqual(["input", "cached", "output"]);
    expect(tokens.days[1].values).toEqual({ input: 100, cached: 50, output: 25 });

    const tokensByModel = buildDailyChart({
      dates,
      rows,
      metric: "tokens",
      split: "model",
      labels
    });
    expect(tokensByModel.series.map((s) => s.key)).toEqual(["glm-5-3"]);
    expect(tokensByModel.days[1].values["glm-5-3"]).toBe(175);
  });

  test("an empty range has no series", () => {
    const chart = buildDailyChart({ dates, rows: [], metric: "credits", split: "model", labels });
    expect(chart.series).toEqual([]);
    expect(chart.days).toHaveLength(3);
  });

  test("extraCreditSpan covers the first to last day paid from Extra credits", () => {
    const chart = buildDailyChart({ dates, rows, metric: "credits", split: "pool", labels });
    expect(extraCreditSpan(chart.days)).toEqual({ from: "2026-10-02", to: "2026-10-02" });
    expect(extraCreditSpan([])).toBeNull();
  });
});

describe("buildMonths", () => {
  test("lists 12 months ending with the current one, which is partial", () => {
    const month = (m: string, source: "app" | "api", pool: "plan" | "extra", credits: number) =>
      ({
        ...row({ date: "", model: "", source, pool, credits, requests: 2 }),
        month: m
      }) as UsageMonthRow;
    const months = buildMonths(
      [
        month("2026-10", "app", "plan", 100),
        month("2026-10", "api", "extra", 40),
        month("2025-11", "app", "plan", 5),
        month("2025-10", "app", "plan", 999)
      ],
      "2026-10"
    );
    expect(months).toHaveLength(12);
    expect(months[0]).toMatchObject({ month: "2025-11", app: 5, api: 0, partial: false });
    expect(months[11]).toEqual({
      month: "2026-10",
      app: 100,
      api: 40,
      requests: 4,
      extraCredits: 40,
      partial: true
    });
  });
});

describe("plan", () => {
  test("plan kind follows the status, not a guess", () => {
    expect(usagePlanKind(status({ product_name: "Free", total_tokens: null }))).toBe("free");
    expect(usagePlanKind(status({ product_name: "Team Plan" }))).toBe("team");
    expect(usagePlanKind(status({ product_name: "Max" }))).toBe("credits");
    expect(usagePlanKind(status({ product_name: "Pro", total_tokens: null }))).toBe("credits");
    expect(usagePlanKind(status({ product_name: null, total_tokens: null }))).toBe("free");
    expect(usagePlanKind(null)).toBe("free");
  });

  test("statusWindow passes the status usage window for paid plans only", () => {
    expect(statusWindow(status({}))).toEqual({
      start: "2026-10-14T05:09:23Z",
      end: "2026-11-14T05:09:23Z"
    });
    expect(statusWindow(status({ product_name: "Free", total_tokens: null }))).toBeNull();
    expect(statusWindow(status({ usage_period_start: undefined }))).toBeNull();
    expect(statusWindow(status({ usage_reset_date: "2026-10-01T00:00:00Z" }))).toBeNull();
  });

  test("periodTicks is weekly for a month and five even steps for a year", () => {
    expect(periodTicks(31)).toEqual([0, 7, 14, 21, 30]);
    expect(periodTicks(30)).toEqual([0, 7, 14, 21, 29]);
    expect(periodTicks(365)).toEqual([0, 91, 182, 273, 364]);
    expect(periodTicks(1)).toEqual([0]);
  });
});

describe("buildPeriodProgress", () => {
  const window = { start: "2026-10-14T05:09:23Z", end: "2026-11-14T05:09:23Z" };
  const now = new Date("2026-10-28T20:00:00Z");

  test("cumulative use, pace and run-out date in local days", () => {
    const progress = buildPeriodProgress({
      window,
      days: [
        { date: "2026-10-14", plan_credits: 700 },
        { date: "2026-10-20", plan_credits: 3000 },
        { date: "2026-10-28", plan_credits: 6800 }
      ],
      used: 10_500,
      allowance: 20_000,
      tz: "America/Chicago",
      now
    });
    expect(progress).not.toBeNull();
    if (!progress) return;
    // Oct 14 00:09 CDT to Nov 13 23:09 CST.
    expect(progress.dates[0]).toBe("2026-10-14");
    expect(progress.dates.at(-1)).toBe("2026-11-13");
    expect(progress.dates).toHaveLength(31);
    expect(progress.todayIndex).toBe(14);
    expect(progress.actual).toHaveLength(15);
    expect(progress.actual[0]).toBe(700);
    expect(progress.actual[5]).toBe(700);
    expect(progress.actual[6]).toBe(3700);
    expect(progress.actual[14]).toBe(10_500);
    expect(progress.pace).toBe(700);
    expect(progress.projected).toBe(10_500 + 700 * 16);
    // 9,500 left at 700 a day: 14 more days.
    expect(progress.runOutDate).toBe("2026-11-11");
    expect(progress.exhausted).toBe(false);
    expect(progress.daysLeft).toBe(17);
    expect(progress.ticks).toEqual([0, 7, 14, 21, 30]);
  });

  test("the last point matches the status meter even when the series lags", () => {
    const lagging = buildPeriodProgress({
      window,
      days: [{ date: "2026-10-14", plan_credits: 500 }],
      used: 900,
      allowance: 20_000,
      tz: "UTC",
      now
    });
    expect(lagging?.actual.at(-1)).toBe(900);

    const ahead = buildPeriodProgress({
      window,
      days: [{ date: "2026-10-14", plan_credits: 1500 }],
      used: 900,
      allowance: 20_000,
      tz: "UTC",
      now
    });
    expect(Math.max(...(ahead?.actual ?? []))).toBe(900);
  });

  test("a projection under the allowance has no run-out date", () => {
    const progress = buildPeriodProgress({
      window,
      days: [],
      used: 1_500,
      allowance: 20_000,
      tz: "UTC",
      now
    });
    expect(progress?.pace).toBe(100);
    expect(progress?.projected).toBe(1_500 + 100 * 17);
    expect(progress?.runOutDate).toBeNull();
  });

  test("used past the allowance is exhausted", () => {
    const progress = buildPeriodProgress({
      window,
      days: [],
      used: 20_000,
      allowance: 20_000,
      tz: "UTC",
      now
    });
    expect(progress?.exhausted).toBe(true);
    expect(progress?.runOutDate).toBeNull();
  });

  test("an invalid window yields nothing", () => {
    expect(
      buildPeriodProgress({
        window: { start: "x", end: "y" },
        days: [],
        used: 0,
        allowance: 1,
        tz: "UTC",
        now
      })
    ).toBeNull();
  });
});

describe("free plan", () => {
  const now = new Date("2026-10-29T01:00:00Z");

  test("messages per UTC day, with today's bar from the live status count", () => {
    const days = buildFreeDays({
      rows: [
        row({ date: "2026-10-29", model: "gpt-oss-120b", requests: 2 }),
        row({ date: "2026-10-28", model: "gpt-oss-120b", requests: 40 }),
        row({ date: "2026-10-28", model: "llama3-3-70b", requests: 5 }),
        row({ date: "2026-10-01", model: "gpt-oss-120b", requests: 9 })
      ],
      now,
      usedToday: 3
    });
    expect(days).toHaveLength(14);
    // 01:00 UTC is still Oct 28 in Chicago; the Free chart uses the UTC day.
    expect(days.at(-1)).toEqual({ date: "2026-10-29", requests: 3, partial: true });
    expect(days.at(-2)).toEqual({ date: "2026-10-28", requests: 45, partial: false });
    expect(days[0].date).toBe("2026-10-16");
  });

  test("used today comes from chats_remaining against the 100 limit", () => {
    expect(freeUsedToday(63)).toBe(37);
    expect(freeUsedToday(-4)).toBe(100);
    expect(freeUsedToday(null)).toBe(0);
  });

  test("the reset is the next UTC midnight, shown in local time", () => {
    expect(nextUtcMidnight(now).toISOString()).toBe("2026-10-30T00:00:00.000Z");
    expect(freeResetLabel(now, "America/Chicago")).toBe(
      "Resets at 7:00 PM your time (midnight UTC)."
    );
    expect(freeResetLabel(now, "UTC")).toBe("Resets at midnight UTC.");
  });
});

describe("niceScale", () => {
  test("picks round steps and whole-number ticks", () => {
    expect(niceScale(1_456)).toEqual({ max: 1_500, ticks: [0, 500, 1_000, 1_500] });
    expect(niceScale(21_000).ticks).toEqual([0, 5_000, 10_000, 15_000, 20_000, 25_000]);
    expect(niceScale(950)).toEqual({ max: 1_000, ticks: [0, 250, 500, 750, 1_000] });
    expect(niceScale(1.04)).toEqual({ max: 2, ticks: [0, 1, 2] });
    expect(niceScale(0).max).toBeGreaterThan(0);
  });
});
