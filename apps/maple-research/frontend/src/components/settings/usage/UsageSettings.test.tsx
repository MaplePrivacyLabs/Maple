import { afterEach, describe, expect, spyOn, test } from "bun:test";
import type { ReactNode } from "react";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import * as router from "@tanstack/react-router";
import * as sdk from "@mapleai/sdk";
import type { BillingStatus } from "@/billing/billingApi";
import * as billing from "@/billing/billingService";
import * as select from "@/components/ui/select";
import type { UsageQuery, UsageRequestsPage, UsageResponse } from "@/billing/usageApi";
import * as localState from "@/state/useLocalState";
import * as platform from "@/utils/platform";
import { UsageSettings } from "./UsageSettings";

function textContent(node: ReactTestInstance): string {
  return node.children
    .map((child) => (typeof child === "string" ? child : textContent(child)))
    .join("");
}

const DAY = 86_400_000;
const now = Date.now();
const windowStart = new Date(now - 10 * DAY).toISOString();
const windowEnd = new Date(now + 20 * DAY).toISOString();
const today = new Date(now).toISOString().slice(0, 10);

const pro: BillingStatus = {
  is_subscribed: true,
  product_id: "pro",
  product_name: "Pro",
  subscription_status: "active",
  current_period_end: null,
  can_chat: true,
  chats_remaining: null,
  payment_provider: "stripe",
  total_tokens: 20_000,
  used_tokens: 10_500,
  usage_reset_date: windowEnd,
  usage_period_start: windowStart,
  api_credit_balance: 18_250
};

const free: BillingStatus = {
  ...pro,
  product_id: "free",
  product_name: "Free",
  payment_provider: null,
  total_tokens: null,
  used_tokens: null,
  chats_remaining: 63,
  api_credit_balance: 0
};

const usage: UsageResponse = {
  range: { kind: "last_30_days", tz: "UTC", start_date: today, end_date: today },
  models: {
    "glm-5-3": { display_name: "GLM-5.3" },
    "llama3-3-70b": { display_name: "Llama 3.3 70B" }
  },
  days: [
    {
      date: today,
      model: "glm-5-3",
      source: "app",
      pool: "plan",
      requests: 12,
      uncached_input_tokens: 1000,
      cached_input_tokens: 3000,
      cached_reported_requests: 12,
      output_tokens: 200,
      credits: 54
    },
    {
      date: today,
      model: "llama3-3-70b",
      source: "app",
      pool: "plan",
      requests: 3,
      uncached_input_tokens: 400,
      cached_input_tokens: 0,
      cached_reported_requests: 0,
      output_tokens: 9,
      credits: 1
    }
  ],
  months: [],
  current_period: { start: windowStart, end: windowEnd, days: [{ date: today, plan_credits: 55 }] }
};

const requests: UsageRequestsPage = {
  models: { "llama3-3-70b": { display_name: "Llama 3.3 70B" } },
  requests: [
    {
      time: new Date(now).toISOString(),
      model: "llama3-3-70b",
      source: "app",
      pool: "plan",
      uncached_input_tokens: 412,
      cached_input_tokens: null,
      output_tokens: 9,
      credits: 1
    }
  ],
  next_cursor: "next"
};

const prices = {
  credits_per_usd: 1000,
  unit: "credits_per_million_tokens",
  models: [
    { id: "glm-5-3", display_name: "GLM-5.3", input: 3600, cached_input: 900, output: 11500 },
    {
      id: "llama3-3-70b",
      display_name: "Llama 3.3 70B",
      input: 3500,
      cached_input: null,
      output: 5500
    }
  ]
};

type FakeService = {
  getBillingStatus: () => Promise<BillingStatus>;
  getUsage: (query: UsageQuery) => Promise<UsageResponse>;
  getUsageRequests: (query: { cursor?: string; limit?: number }) => Promise<UsageRequestsPage>;
  getUsagePrices: () => Promise<typeof prices>;
  getTeamStatus: () => Promise<unknown>;
};

describe("Usage settings page", () => {
  let renderer: ReactTestRenderer | null = null;
  const cleanup: Array<() => void> = [];

  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    renderer = null;
    cleanup
      .splice(0)
      .reverse()
      .forEach((restore) => restore());
  });

  async function mount(service: Partial<FakeService>, cached: BillingStatus | null = null) {
    const fake: FakeService = {
      getBillingStatus: () => new Promise(() => {}),
      getUsage: () => Promise.resolve(usage),
      getUsageRequests: () => Promise.resolve(requests),
      getUsagePrices: () => Promise.resolve(prices),
      getTeamStatus: () => Promise.resolve({ has_team_subscription: false, team_created: false }),
      ...service
    };
    const spies = [
      spyOn(sdk, "useOpenSecret").mockReturnValue({
        auth: {
          user: { user: { id: "account-fixture", email: "a@example.invalid" } },
          loading: false
        }
      } as ReturnType<typeof sdk.useOpenSecret>),
      spyOn(billing, "getBillingService").mockReturnValue(
        fake as unknown as ReturnType<typeof billing.getBillingService>
      ),
      spyOn(router, "Link").mockImplementation((({
        children,
        to
      }: {
        children: ReactNode;
        to: string;
      }) => <a href={to}>{children}</a>) as unknown as typeof router.Link),
      // Radix Select needs a DOM; the range picker isn't under test here.
      spyOn(select, "Select").mockImplementation((() => (
        <div data-range-select />
      )) as unknown as typeof select.Select),
      // Spied rather than provided: another test file replaces this module.
      spyOn(localState, "useBillingState").mockReturnValue({
        billingStatus: cached,
        setBillingStatus: () => {}
      }),
      spyOn(platform, "isMobile").mockReturnValue(false),
      spyOn(platform, "isAndroid").mockReturnValue(false)
    ];
    cleanup.push(...spies.map((spy) => () => spy.mockRestore()));
    const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } });
    cleanup.push(() => client.clear());
    await act(async () => {
      renderer = create(
        <QueryClientProvider client={client}>
          <UsageSettings />
        </QueryClientProvider>
      );
    });
    // Let the dependent usage queries settle.
    for (let i = 0; i < 5; i += 1) {
      await act(async () => {
        await new Promise((resolve) => setTimeout(resolve, 0));
      });
    }
    return fake;
  }

  const text = () => textContent(renderer!.root);

  test("shows a loading state until the plan loads", async () => {
    await mount({});
    expect(text()).toContain("Loading usage...");
  });

  test("offers a retry when the plan can't load", async () => {
    let calls = 0;
    await mount({
      getBillingStatus: () => {
        calls += 1;
        return Promise.reject(new Error("billing down"));
      }
    });
    expect(text()).toContain("Couldn't load your plan.");
    const retry = renderer!.root
      .findAllByType("button")
      .find((b) => textContent(b).includes("Try again"));
    await act(async () => {
      retry!.props.onClick();
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
    expect(calls).toBe(2);
  });

  test("a paid plan shows the status meter and history for the status window", async () => {
    const queries: UsageQuery[] = [];
    await mount(
      {
        getBillingStatus: () => Promise.resolve(pro),
        getUsage: (query) => {
          queries.push(query);
          return Promise.resolve(usage);
        }
      },
      pro
    );
    expect(queries).toHaveLength(1);
    expect(queries[0]).toMatchObject({
      range: "last_30_days",
      periodStart: windowStart,
      periodEnd: windowEnd
    });
    const content = text();
    for (const expected of [
      "Your whole account.",
      "10,500",
      "of 20,000 plan credits",
      "18,250 Extra credits",
      "Buy credits",
      "Usage in this range",
      "Daily credits",
      "By model",
      "GLM-5.3",
      "Recent requests",
      "Load more",
      "Paid from",
      "Same as input",
      "Last 12 months",
      "About these numbers"
    ]) {
      expect(content).toContain(expected);
    }
    // Llama reported no cached tokens: the model row and the request show "—".
    expect(content).toContain("—");
    // Monthly history sits right under the model table, above the request log.
    const order = ["By model", "Last 12 months", "Recent requests", "Current model prices"].map(
      (heading) => content.indexOf(heading)
    );
    expect(order).toEqual([...order].sort((a, b) => a - b));
  });

  test("recent requests open with 10 rows and load 50 more at a time", async () => {
    const pages: Array<{ cursor?: string; limit?: number }> = [];
    await mount(
      {
        getBillingStatus: () => Promise.resolve(pro),
        getUsageRequests: (query) => {
          pages.push(query);
          return Promise.resolve({ ...requests, next_cursor: pages.length === 1 ? "next" : null });
        }
      },
      pro
    );
    expect(pages).toEqual([{ cursor: undefined, limit: 10 }]);
    const loadMore = renderer!.root.findAll(
      (node) => node.props.onClick && textContent(node) === "Load more"
    )[0];
    await act(async () => {
      loadMore.props.onClick();
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
    expect(pages).toEqual([
      { cursor: undefined, limit: 10 },
      { cursor: "next", limit: 50 }
    ]);
  });

  test("a usage failure keeps the plan meter and offers a retry", async () => {
    await mount(
      {
        getBillingStatus: () => Promise.resolve(pro),
        getUsage: () => Promise.reject(new Error("usage down"))
      },
      pro
    );
    const content = text();
    expect(content).toContain("10,500");
    expect(content).toContain("Couldn't load usage for this range.");
    expect(content).toContain("The daily chart for this period isn't available right now.");
  });

  test("Free counts messages today and reads UTC days without a plan window", async () => {
    const queries: UsageQuery[] = [];
    await mount(
      {
        getBillingStatus: () => Promise.resolve(free),
        getUsage: (query) => {
          queries.push(query);
          return Promise.resolve({ ...usage, current_period: null });
        }
      },
      free
    );
    const content = text();
    expect(content).toContain("of 100 messages");
    expect(content).toContain("37");
    expect(content).toContain("Messages per UTC day");
    expect(content).not.toContain("Buy credits");
    expect(content).not.toContain("Last 12 months");
    expect(queries.some((q) => q.tz === "UTC" && q.range === "last_30_days")).toBe(true);
    expect(queries.every((q) => q.periodStart === undefined)).toBe(true);
  });
});
