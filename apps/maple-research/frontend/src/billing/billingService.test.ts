import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import type { OpenSecretContextType } from "@mapleai/sdk";
import { BillingService, BillingSessionChangedError, type BillingIdentity } from "./billingService";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((accept, fail) => {
    resolve = accept;
    reject = fail;
  });
  return { promise, resolve, reject };
}

function json(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "Content-Type": "application/json" }
  });
}

const originalFetch = globalThis.fetch;
const originalWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
const originalStorage = Object.getOwnPropertyDescriptor(globalThis, "sessionStorage");
const originalBillingUrl = process.env.VITE_MAPLE_BILLING_API_URL;
let quietErrors: ReturnType<typeof spyOn>;
let stored: Map<string, string>;

beforeEach(() => {
  process.env.VITE_MAPLE_BILLING_API_URL = "https://billing.example.test";
  quietErrors = spyOn(console, "error").mockImplementation(() => {});
  stored = new Map([["maple_billing_token", "legacy-unscoped-test-token"]]);
  Object.defineProperty(globalThis, "sessionStorage", {
    configurable: true,
    value: {
      getItem: (key: string) => stored.get(key) ?? null,
      removeItem: (key: string) => stored.delete(key),
      setItem: (key: string, value: string) => stored.set(key, value)
    }
  });
});

afterEach(() => {
  globalThis.fetch = originalFetch;
  if (originalWindow) Object.defineProperty(globalThis, "window", originalWindow);
  else Reflect.deleteProperty(globalThis, "window");
  quietErrors.mockRestore();
  if (originalStorage) Object.defineProperty(globalThis, "sessionStorage", originalStorage);
  else Reflect.deleteProperty(globalThis, "sessionStorage");
  if (originalBillingUrl === undefined) delete process.env.VITE_MAPLE_BILLING_API_URL;
  else process.env.VITE_MAPLE_BILLING_API_URL = originalBillingUrl;
});

function fixture() {
  let identity: BillingIdentity = {
    apiOrigin: "https://api.example.test",
    principalId: "account-a",
    revision: 1
  };
  let mints = 0;
  let mint: () => Promise<{ token: string }> = async () => ({
    token: `fixture-billing-token-${++mints}`
  });
  let request: (token: string, url: string) => Promise<Response> = async () =>
    json({ product_name: "Pro", payment_provider: "stripe" });
  const sent: { token: string; url: string }[] = [];
  const context = (userId: string | null): OpenSecretContextType =>
    ({
      apiUrl: "https://api.example.test",
      auth: { user: userId ? { user: { id: userId } } : undefined },
      generateThirdPartyToken: () => mint()
    }) as OpenSecretContextType;
  globalThis.fetch = (async (input, init) => {
    const token = new Headers(init?.headers).get("Authorization") ?? "";
    sent.push({ token, url: String(input) });
    return request(token, String(input));
  }) as typeof fetch;
  const service = new BillingService(context("account-a"), () => identity);
  return {
    service,
    sent,
    mints: () => mints,
    setMint: (value: typeof mint) => (mint = value),
    setRequest: (value: typeof request) => (request = value),
    setIdentity: (value: BillingIdentity) => (identity = value),
    switchAccount: (userId: string | null, revision: number) => {
      identity = { ...identity, principalId: userId, revision };
      service.updateOpenSecret(context(userId));
    },
    refreshContext: () => service.updateOpenSecret(context(identity.principalId))
  };
}

describe("BillingService credential ownership", () => {
  test("discards legacy storage, coalesces minting, and caches only for this credential revision", async () => {
    const f = fixture();
    const minted = deferred<{ token: string }>();
    let mints = 0;
    f.setMint(() => {
      mints++;
      return minted.promise;
    });
    const first = f.service.getBillingStatus();
    const second = f.service.getTeamStatus();
    expect(stored.has("maple_billing_token")).toBe(false);
    expect(mints).toBe(1);
    minted.resolve({ token: "fixture-current-token" });
    await Promise.all([first, second]);
    f.refreshContext();
    await f.service.getApiCreditBalance();
    expect(mints).toBe(1);
    expect(f.sent.map((call) => call.token)).toEqual(Array(3).fill("Bearer fixture-current-token"));
    expect(stored.has("maple_billing_token")).toBe(false);
  });

  test("clearToken revokes a pending mint before it can send or repopulate the cache", async () => {
    const f = fixture();
    const minted = deferred<{ token: string }>();
    f.setMint(() => minted.promise);
    const pending = f.service.getBillingStatus();
    void pending.catch(() => {});
    f.service.clearToken();
    f.setMint(async () => ({ token: "fixture-after-clear" }));
    await f.service.getBillingStatus();
    minted.resolve({ token: "fixture-stale-token" });
    await expect(pending).rejects.toBeInstanceOf(BillingSessionChangedError);
    await f.service.getBillingStatus();
    expect(f.sent.map((call) => call.token)).toEqual([
      "Bearer fixture-after-clear",
      "Bearer fixture-after-clear"
    ]);
    expect(stored.has("maple_billing_token")).toBe(false);
  });

  test("a captured caller guard is revoked by clearToken even for the same SDK revision", async () => {
    const f = fixture();
    const oldGuard = f.service.captureSessionGuard();
    await f.service.getBillingStatus();
    oldGuard();
    f.service.clearToken();
    const newGuard = f.service.captureSessionGuard();
    expect(oldGuard).toThrow(BillingSessionChangedError);
    newGuard();
    await f.service.getBillingStatus();
    newGuard();
    expect(f.mints()).toBe(2);
  });

  test("a mint begun by A cannot be sent after B takes ownership", async () => {
    const f = fixture();
    const minted = deferred<{ token: string }>();
    f.setMint(() => minted.promise);
    const pending = f.service.getBillingStatus();
    void pending.catch(() => {});
    f.switchAccount("account-b", 2);
    f.setMint(async () => ({ token: "fixture-account-b" }));
    await f.service.getBillingStatus();
    minted.resolve({ token: "fixture-account-a" });
    await expect(pending).rejects.toBeInstanceOf(BillingSessionChangedError);
    expect(f.sent.map((call) => call.token)).toEqual(["Bearer fixture-account-b"]);
  });

  test("clearToken rejects an already sent response instead of publishing it after logout", async () => {
    const f = fixture();
    const requested = deferred<void>();
    const response = deferred<Response>();
    f.setRequest(() => {
      requested.resolve();
      return response.promise;
    });
    const pending = f.service.getBillingStatus();
    void pending.catch(() => {});
    await requested.promise;
    f.service.clearToken();
    response.resolve(json({ product_name: "Old private billing status" }));
    await expect(pending).rejects.toBeInstanceOf(BillingSessionChangedError);
  });

  test("SDK refresh fences a response even before React updates the context", async () => {
    const f = fixture();
    const requested = deferred<void>();
    const response = deferred<Response>();
    f.setRequest(() => {
      requested.resolve();
      return response.promise;
    });
    const pending = f.service.getBillingStatus();
    void pending.catch(() => {});
    await requested.promise;
    f.setIdentity({ apiOrigin: "https://api.example.test", principalId: "account-a", revision: 2 });
    response.resolve(json({ product_name: "Old revision" }));
    await expect(pending).rejects.toBeInstanceOf(BillingSessionChangedError);
    f.setRequest(async () => json({ product_name: "Current revision" }));
    expect((await f.service.getBillingStatus()).product_name).toBe("Current revision");
    expect(f.mints()).toBe(2);
  });

  test("rejects a stale React user and a different SDK principal without minting", async () => {
    const f = fixture();
    f.setIdentity({ apiOrigin: "https://api.example.test", principalId: "account-b", revision: 2 });
    await expect(f.service.getBillingStatus()).rejects.toBeInstanceOf(BillingSessionChangedError);
    expect(f.mints()).toBe(0);
    expect(f.sent).toEqual([]);
  });

  test("A to B to A does not revive the first A request", async () => {
    const f = fixture();
    const requested = deferred<void>();
    const response = deferred<Response>();
    f.setRequest(() => {
      requested.resolve();
      return response.promise;
    });
    const pending = f.service.getBillingStatus();
    void pending.catch(() => {});
    await requested.promise;
    f.switchAccount("account-b", 2);
    f.switchAccount("account-a", 3);
    response.resolve(json({ product_name: "Old A" }));
    await expect(pending).rejects.toBeInstanceOf(BillingSessionChangedError);
  });

  test("a late A 401 cannot discard B's cached token or retry as B", async () => {
    const f = fixture();
    const requested = deferred<void>();
    const response = deferred<Response>();
    f.setRequest(() => {
      requested.resolve();
      return response.promise;
    });
    const pending = f.service.getBillingStatus();
    void pending.catch(() => {});
    await requested.promise;
    f.switchAccount("account-b", 2);
    f.setRequest(async () => json({ product_name: "B" }));
    await f.service.getBillingStatus();
    response.resolve(json({ error: "fixture expired" }, 401));
    await expect(pending).rejects.toBeInstanceOf(BillingSessionChangedError);
    await f.service.getBillingStatus();
    expect(f.mints()).toBe(2);
    expect(f.sent.map((call) => call.token)).toEqual([
      "Bearer fixture-billing-token-1",
      "Bearer fixture-billing-token-2",
      "Bearer fixture-billing-token-2"
    ]);
  });

  test("concurrent 401 responses share the replacement mint and retry only once", async () => {
    const f = fixture();
    const firstRequest = deferred<Response>();
    const secondRequest = deferred<Response>();
    const sentTwice = deferred<void>();
    let calls = 0;
    f.setRequest(() => {
      calls++;
      if (calls === 1) return firstRequest.promise;
      if (calls === 2) {
        sentTwice.resolve();
        return secondRequest.promise;
      }
      return Promise.resolve(json({ product_name: "Current token" }));
    });
    const first = f.service.getBillingStatus();
    const second = f.service.getBillingStatus();
    await sentTwice.promise;
    firstRequest.resolve(json({ error: "fixture expired" }, 401));
    expect((await first).product_name).toBe("Current token");
    secondRequest.resolve(json({ error: "fixture expired" }, 401));
    expect((await second).product_name).toBe("Current token");
    expect(f.mints()).toBe(2);
    expect(f.sent.map((call) => call.token)).toEqual([
      "Bearer fixture-billing-token-1",
      "Bearer fixture-billing-token-1",
      "Bearer fixture-billing-token-2",
      "Bearer fixture-billing-token-2"
    ]);
    f.setRequest(async () => json({ error: "fixture unauthorized" }, 401));
    await expect(f.service.getBillingStatus()).rejects.toThrow("Unauthorized");
    expect(f.mints()).toBe(3);
    expect(f.sent).toHaveLength(6);
  });

  test("a logout during the retry mint prevents the retried API call", async () => {
    const f = fixture();
    await f.service.getBillingStatus();
    const minted = deferred<{ token: string }>();
    const mintStarted = deferred<void>();
    f.setMint(() => {
      mintStarted.resolve();
      return minted.promise;
    });
    f.setRequest(async () => json({ error: "fixture expired" }, 401));
    const pending = f.service.getBillingStatus();
    void pending.catch(() => {});
    await mintStarted.promise;
    f.switchAccount(null, 2);
    minted.resolve({ token: "fixture-stale-retry" });
    await expect(pending).rejects.toBeInstanceOf(BillingSessionChangedError);
    expect(f.sent).toHaveLength(2);
  });

  test("public catalog requests still work signed out and never send authorization", async () => {
    const f = fixture();
    f.switchAccount(null, 2);
    f.setRequest(async () => json([]));
    expect(await f.service.getProducts("4.0.0")).toEqual([]);
    expect(f.mints()).toBe(0);
    expect(f.sent[0].token).toBe("");
    expect(f.sent[0].url).toContain("/v1/maple/products?version=4.0.0");
    await expect(f.service.getBillingStatus()).rejects.toBeInstanceOf(BillingSessionChangedError);
    expect(f.sent).toHaveLength(1);
  });

  for (const provider of ["stripe", "zaprite"] as const) {
    test(`${provider} checkout cannot navigate after account change while decoding its response`, async () => {
      const f = fixture();
      const location = { href: "https://app.example.test/pricing" };
      Object.defineProperty(globalThis, "window", { configurable: true, value: { location } });
      const bodyStarted = deferred<void>();
      const body = deferred<{ checkout_url: string }>();
      f.setRequest(async () => {
        const response = json({});
        response.json = () => {
          bodyStarted.resolve();
          return body.promise;
        };
        return response;
      });
      const checkout = () =>
        provider === "stripe"
          ? f.service.createCheckoutSession(
              "fixture@example.test",
              "fixture-product",
              "https://app.example.test/success",
              "https://app.example.test/cancel"
            )
          : f.service.createZapriteCheckoutSession(
              "fixture@example.test",
              "fixture-product",
              "https://app.example.test/success"
            );
      const pending = checkout();
      void pending.catch(() => {});
      await bodyStarted.promise;
      f.switchAccount("account-b", 2);
      body.resolve({ checkout_url: "https://checkout.example.test/old-account" });
      await expect(pending).rejects.toBeInstanceOf(BillingSessionChangedError);
      expect(location.href).toBe("https://app.example.test/pricing");

      f.setRequest(async () => json({ checkout_url: "https://checkout.example.test/new-account" }));
      await checkout();
      expect(location.href).toBe("https://checkout.example.test/new-account");
    });
  }

  test("rejects credentials for the wrong API origin and malformed revisions", async () => {
    const f = fixture();
    for (const identity of [
      { apiOrigin: "https://other.example.test", principalId: "account-a", revision: 1 },
      { apiOrigin: "https://api.example.test", principalId: "account-a", revision: -1 },
      { apiOrigin: "https://api.example.test", principalId: "account-a", revision: NaN }
    ]) {
      f.setIdentity(identity);
      await expect(f.service.getBillingStatus()).rejects.toBeInstanceOf(BillingSessionChangedError);
    }
    expect(f.mints()).toBe(0);
    expect(f.sent).toEqual([]);
  });
});
