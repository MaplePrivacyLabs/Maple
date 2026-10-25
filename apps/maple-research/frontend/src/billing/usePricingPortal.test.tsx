import { afterEach, describe, expect, mock, spyOn, test } from "bun:test";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import type { BillingStatus, BillingSubscription } from "./billingApi";
import { usePricingPortal, type PricingPortalInput } from "./usePricingPortal";

const stripe: BillingStatus = {
  is_subscribed: true,
  stripe_customer_id: "cus_fixture",
  product_id: "pro",
  product_name: "Pro",
  subscription_status: "active",
  current_period_end: null,
  can_chat: true,
  chats_remaining: null,
  payment_provider: "stripe",
  total_tokens: null,
  used_tokens: null,
  usage_reset_date: null
};
const stripeSubscription: BillingSubscription = {
  provider: "stripe",
  plan: "Pro",
  state: "active",
  manage: "portal",
  renews_at: null
};
const free: BillingStatus = {
  ...stripe,
  is_subscribed: true, // The legacy Free response can still set this flag.
  stripe_customer_id: null,
  product_id: "free",
  product_name: "Free",
  payment_provider: null
};
const portalUrl = "https://billing.stripe.com/p/session/fixture";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((accept) => {
    resolve = accept;
  });
  return { promise, resolve };
}

describe("pricing portal requests", () => {
  let renderer: ReactTestRenderer | undefined;
  let client: QueryClient | undefined;
  const silence = () => spyOn(console, "error").mockImplementation(() => {});
  let consoleError: ReturnType<typeof silence> | undefined;

  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    client?.clear();
    consoleError?.mockRestore();
    renderer = undefined;
    client = undefined;
    consoleError = undefined;
  });

  function mount(
    input: PricingPortalInput,
    requestPortal = mock<(userId: string) => Promise<string>>(async () => portalUrl),
    cachedUrl?: null
  ) {
    client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: Infinity } } });
    if (cachedUrl === null) client.setQueryData(["portalUrl", input.userId], null);
    let view!: ReturnType<typeof usePricingPortal>;
    function Harness({ input }: { input: PricingPortalInput }) {
      view = usePricingPortal(input, requestPortal);
      return null;
    }
    const tree = (input: PricingPortalInput) => (
      <QueryClientProvider client={client!}>
        <Harness input={input} />
      </QueryClientProvider>
    );
    act(() => {
      renderer = create(tree(input));
    });
    return {
      requestPortal,
      get view() {
        return view;
      },
      update(input: PricingPortalInput) {
        act(() => renderer!.update(tree(input)));
      }
    };
  }

  async function settle() {
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
  }

  test("loading billing status and a confirmed Free account never request a portal", async () => {
    const f = mount({ userId: "account", status: undefined, statusConfirmed: false });
    await settle();
    expect(f.requestPortal).not.toHaveBeenCalled();
    f.update({ userId: "account", status: free, statusConfirmed: true });
    await settle();
    expect(f.requestPortal).not.toHaveBeenCalled();
    expect(f.view.isAwaitingPortal).toBe(false);
  });

  test.each(["zaprite", "subscription_pass", "apple"] as const)(
    "%s does not prefetch even with an old Stripe customer and management row",
    async (provider) => {
      const f = mount({
        userId: "account",
        status: { ...stripe, payment_provider: provider, subscriptions: [stripeSubscription] },
        statusConfirmed: true
      });
      await settle();
      expect(f.requestPortal).not.toHaveBeenCalled();
      expect(f.view.isAwaitingPortal).toBe(false);
    }
  );

  test.each(["team_admin", "https://example.invalid", "unknown"])(
    "a Stripe management hint of %s does not request a portal",
    async (manage) => {
      const f = mount({
        userId: "account",
        status: { ...stripe, subscriptions: [{ ...stripeSubscription, manage }] },
        statusConfirmed: true
      });
      await settle();
      expect(f.requestPortal).not.toHaveBeenCalled();
    }
  );

  test("an authoritative empty list is not replaced by the legacy customer fallback", async () => {
    const f = mount({
      userId: "account",
      status: { ...stripe, subscriptions: [] },
      statusConfirmed: true
    });
    await settle();
    expect(f.requestPortal).not.toHaveBeenCalled();
  });

  test("explicit Stripe management keeps working without a client-visible customer ID", async () => {
    const f = mount({
      userId: "account",
      status: { ...stripe, stripe_customer_id: null, subscriptions: [stripeSubscription] },
      statusConfirmed: true
    });
    await settle();
    expect(f.requestPortal).toHaveBeenCalledWith("account");
    expect(f.view.portalUrl).toBe(portalUrl);
  });

  test("legacy Stripe waits for confirmed status and holds management while its URL loads", async () => {
    const pending = deferred<string>();
    const request = mock<(userId: string) => Promise<string>>(() => pending.promise);
    const f = mount({ userId: "account", status: stripe, statusConfirmed: false }, request);
    expect(request).not.toHaveBeenCalled();
    f.update({ userId: "account", status: stripe, statusConfirmed: true });
    expect(request).toHaveBeenCalledTimes(1);
    expect(f.view.isAwaitingPortal).toBe(true);
    await act(async () => pending.resolve(portalUrl));
    await settle();
    expect(f.view.portalUrl).toBe(portalUrl);
    expect(f.view.isAwaitingPortal).toBe(false);
  });

  test("a cached null URL still holds management during a replacement request", async () => {
    const pending = deferred<string>();
    const f = mount(
      { userId: "account", status: stripe, statusConfirmed: true },
      mock<(userId: string) => Promise<string>>(() => pending.promise),
      null
    );
    expect(f.view.isAwaitingPortal).toBe(true);
    await act(async () => pending.resolve(portalUrl));
    await settle();
    expect(f.view.isAwaitingPortal).toBe(false);
  });

  test("a failed request keeps the existing null result and releases the loading guard", async () => {
    consoleError = silence();
    const f = mount(
      { userId: "account", status: stripe, statusConfirmed: true },
      mock<(userId: string) => Promise<string>>(async () => {
        throw new Error("fixture failure");
      })
    );
    await settle();
    expect(f.view.portalUrl).toBeNull();
    expect(f.view.isAwaitingPortal).toBe(false);
    expect(f.requestPortal).toHaveBeenCalledTimes(1);
  });

  test("logout and an older account's delayed response do not publish a portal for another owner", async () => {
    const old = deferred<string>();
    const current = deferred<string>();
    const request = mock((userId: string) => (userId === "old" ? old.promise : current.promise));
    const f = mount({ userId: "old", status: stripe, statusConfirmed: true }, request);
    f.update({ userId: undefined, status: undefined, statusConfirmed: false });
    expect(f.view.portalUrl).toBeUndefined();
    f.update({ userId: "current", status: stripe, statusConfirmed: true });
    await act(async () => old.resolve("https://billing.stripe.com/p/session/old-owner"));
    await settle();
    expect(f.view.portalUrl).toBeUndefined();
    expect(f.view.isAwaitingPortal).toBe(true);
    await act(async () => current.resolve(portalUrl));
    await settle();
    expect(f.view.portalUrl).toBe(portalUrl);
    expect(request.mock.calls.map(([owner]) => owner)).toEqual(["old", "current"]);
  });
});
