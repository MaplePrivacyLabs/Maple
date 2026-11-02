import { parseBillingApiError } from "./zapriteUpgrade";

// Usage history served by the billing API. Credits are what billing charged at
// the time; token counts are split so input + cached input + output add up.

export type UsageRange = "last_30_days" | `month:${string}`;
export type UsageSource = "app" | "api";
export type UsagePool = "plan" | "extra";

export type UsageModelLabels = Record<string, { display_name?: string | null }>;

export type UsageCounts = {
  requests: number;
  uncached_input_tokens: number;
  cached_input_tokens: number;
  /** Requests that reported a cached count. 0 means "not reported", not "none". */
  cached_reported_requests: number;
  output_tokens: number;
  credits: number;
};

export type UsageDayRow = UsageCounts & {
  /** Local calendar date (YYYY-MM-DD) in `range.tz`. */
  date: string;
  model: string;
  source: UsageSource;
  pool: UsagePool;
};

export type UsageMonthRow = UsageCounts & {
  /** Local calendar month (YYYY-MM). */
  month: string;
  source: UsageSource;
  pool: UsagePool;
};

export type UsageCurrentPeriod = {
  start: string;
  end: string;
  days: Array<{ date: string; plan_credits: number }>;
};

export type UsageResponse = {
  range: { kind: string; tz: string; start_date: string; end_date: string };
  models: UsageModelLabels;
  days: UsageDayRow[];
  months: UsageMonthRow[];
  current_period: UsageCurrentPeriod | null;
};

export type UsageRequest = {
  time: string;
  model: string;
  source: UsageSource;
  pool: UsagePool;
  uncached_input_tokens: number;
  /** null when the provider did not report a cached count. */
  cached_input_tokens: number | null;
  output_tokens: number;
  credits: number;
};

export type UsageRequestsPage = {
  models: UsageModelLabels;
  requests: UsageRequest[];
  next_cursor: string | null;
};

export type UsagePrice = {
  id: string;
  display_name: string;
  input: number;
  /** null: no cached discount, cached tokens bill as input. */
  cached_input: number | null;
  output: number;
  price_may_vary?: boolean;
};

export type UsagePricesResponse = {
  credits_per_usd: number;
  unit: string;
  models: UsagePrice[];
};

export type UsageQuery = {
  range: UsageRange;
  tz: string;
  /** The status usage window; billing returns `current_period` for it. */
  periodStart?: string;
  periodEnd?: string;
};

function billingUrl(path: string): URL {
  return new URL(`${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple${path}`);
}

async function readJson<T>(response: Response, what: string): Promise<T> {
  if (!response.ok) {
    const errorText = await response.text();
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    throw new Error(`Failed to load ${what}: ${parseBillingApiError(errorText).error}`);
  }
  return response.json() as Promise<T>;
}

function authHeaders(thirdPartyToken: string): HeadersInit {
  return {
    Authorization: `Bearer ${thirdPartyToken}`,
    "Content-Type": "application/json"
  };
}

export async function fetchUsage(
  thirdPartyToken: string,
  query: UsageQuery
): Promise<UsageResponse> {
  const url = billingUrl("/usage");
  url.searchParams.set("range", query.range);
  url.searchParams.set("tz", query.tz);
  if (query.periodStart && query.periodEnd) {
    url.searchParams.set("period_start", query.periodStart);
    url.searchParams.set("period_end", query.periodEnd);
  }
  const response = await fetch(url.toString(), { headers: authHeaders(thirdPartyToken) });
  const body = await readJson<Partial<UsageResponse>>(response, "usage");
  return {
    range: body.range ?? { kind: query.range, tz: query.tz, start_date: "", end_date: "" },
    models: body.models ?? {},
    days: body.days ?? [],
    months: body.months ?? [],
    current_period: body.current_period
      ? { ...body.current_period, days: body.current_period.days ?? [] }
      : null
  };
}

export async function fetchUsageRequests(
  thirdPartyToken: string,
  query: { cursor?: string; limit?: number } = {}
): Promise<UsageRequestsPage> {
  const url = billingUrl("/usage/requests");
  if (query.limit !== undefined) url.searchParams.set("limit", String(query.limit));
  if (query.cursor) url.searchParams.set("cursor", query.cursor);
  const response = await fetch(url.toString(), { headers: authHeaders(thirdPartyToken) });
  const body = await readJson<Partial<UsageRequestsPage>>(response, "recent requests");
  return {
    models: body.models ?? {},
    requests: body.requests ?? [],
    next_cursor: body.next_cursor ?? null
  };
}

/** Public: current customer prices, no account token. */
export async function fetchUsagePrices(): Promise<UsagePricesResponse> {
  const response = await fetch(billingUrl("/usage/prices").toString());
  const body = await readJson<Partial<UsagePricesResponse>>(response, "model prices");
  return {
    credits_per_usd: body.credits_per_usd ?? 1000,
    unit: body.unit ?? "credits_per_million_tokens",
    models: body.models ?? []
  };
}
