import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import { fetchUsage, fetchUsagePrices, fetchUsageRequests } from "./usageApi";

const BILLING = "https://billing.example.test";

describe("usage API", () => {
  let previous: string | undefined;
  let fetchSpy: ReturnType<typeof spyOn<typeof globalThis, "fetch">> | undefined;

  beforeEach(() => {
    previous = process.env.VITE_MAPLE_BILLING_API_URL;
    process.env.VITE_MAPLE_BILLING_API_URL = BILLING;
  });

  afterEach(() => {
    if (previous === undefined) delete process.env.VITE_MAPLE_BILLING_API_URL;
    else process.env.VITE_MAPLE_BILLING_API_URL = previous;
    fetchSpy?.mockRestore();
    fetchSpy = undefined;
  });

  const respond = (body: unknown, status = 200) => {
    fetchSpy = spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(typeof body === "string" ? body : JSON.stringify(body), { status })
    );
    return fetchSpy;
  };

  const requested = () => {
    const [url, init] = fetchSpy!.mock.calls[0] as [string, RequestInit | undefined];
    return { url: new URL(url), headers: new Headers(init?.headers) };
  };

  test("usage sends the range, zone and status window with the account token", async () => {
    respond({ range: { kind: "last_30_days", tz: "America/Chicago" }, days: [{ date: "x" }] });
    const usage = await fetchUsage("token-1", {
      range: "month:2026-09",
      tz: "America/Chicago",
      periodStart: "2026-10-14T05:09:23.123456Z",
      periodEnd: "2026-11-14T05:09:23+00:00"
    });
    const { url, headers } = requested();
    expect(url.origin + url.pathname).toBe(`${BILLING}/v1/maple/usage`);
    expect(url.searchParams.get("range")).toBe("month:2026-09");
    expect(url.searchParams.get("tz")).toBe("America/Chicago");
    expect(url.searchParams.get("period_start")).toBe("2026-10-14T05:09:23.123456Z");
    expect(url.searchParams.get("period_end")).toBe("2026-11-14T05:09:23+00:00");
    expect(headers.get("Authorization")).toBe("Bearer token-1");
    // Missing collections are normalized so the page never reads undefined.
    expect(usage.models).toEqual({});
    expect(usage.months).toEqual([]);
    expect(usage.days).toHaveLength(1);
    expect(usage.current_period).toBeNull();
  });

  test("usage omits a half-specified window", async () => {
    respond({});
    await fetchUsage("t", {
      range: "last_30_days",
      tz: "UTC",
      periodStart: "2026-10-14T00:00:00Z"
    });
    const { url } = requested();
    expect(url.searchParams.has("period_start")).toBe(false);
    expect(url.searchParams.has("period_end")).toBe(false);
  });

  test("a 401 throws Unauthorized so BillingService can retry with a new token", async () => {
    respond("expired", 401);
    await expect(fetchUsage("t", { range: "last_30_days", tz: "UTC" })).rejects.toThrow(
      "Unauthorized"
    );
  });

  test("other errors carry billing's message", async () => {
    respond(JSON.stringify({ error: "tz must be an IANA time zone name" }), 400);
    await expect(fetchUsage("t", { range: "last_30_days", tz: "Nope" })).rejects.toThrow(
      "Failed to load usage: tz must be an IANA time zone name"
    );
  });

  test("recent requests page with an opaque cursor", async () => {
    respond({ requests: [], next_cursor: "abc+/=" });
    const page = await fetchUsageRequests("t", { cursor: "abc+/=", limit: 50 });
    const { url } = requested();
    expect(url.pathname).toBe("/v1/maple/usage/requests");
    expect(url.searchParams.get("cursor")).toBe("abc+/=");
    expect(url.searchParams.get("limit")).toBe("50");
    expect(page).toEqual({ models: {}, requests: [], next_cursor: "abc+/=" });
  });

  test("prices are public and send no credentials", async () => {
    respond({ models: [{ id: "glm-5-3", input: 3600, cached_input: null, output: 11500 }] });
    const prices = await fetchUsagePrices();
    const { url, headers } = requested();
    expect(url.pathname).toBe("/v1/maple/usage/prices");
    expect(headers.has("Authorization")).toBe(false);
    expect(prices.models[0].cached_input).toBeNull();
    expect(prices.credits_per_usd).toBe(1000);
  });
});
