import { describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import {
  storeKit,
  StoreKitRecoveryError,
  type SignedStoreKitTransaction,
  type StoreKitBridge,
  type StoreKitPurchaseResult
} from "@/services/storeKitService";
import { AppleBillingApiError, type AppleTransactionResponse } from "./appleBillingApi";
import { AppleBillingRetryPolicy } from "./appleBillingRetryPolicy";
import {
  AppleBillingSession,
  AppleBillingSessionChangedError,
  type AppleBillingIdentity,
  type AppleBillingSessionOptions
} from "./appleBillingSession";

const accountToken = "11111111-1111-4111-8111-111111111111";
const transaction: SignedStoreKitTransaction = {
  transactionId: "9007199254740993",
  originalTransactionId: "9007199254740993",
  productId: "cloud.opensecret.maple.pro.monthly",
  jws: "opaque-test-transaction"
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((accept, fail) => {
    resolve = accept;
    reject = fail;
  });
  return { promise, resolve, reject };
}

function response(id = transaction.transactionId): AppleTransactionResponse {
  return {
    acknowledged_transaction_id: id,
    payment_provider: "stripe",
    is_subscribed: true,
    stripe_customer_id: null,
    product_id: "existing-max",
    product_name: "Max",
    subscription_status: "active",
    current_period_end: null,
    can_chat: true,
    chats_remaining: null,
    total_tokens: null,
    used_tokens: null,
    usage_reset_date: null,
    ios_iap_enabled: false,
    ios_us_external_link_enabled: false,
    subscriptions: [
      { provider: "apple", plan: "Pro", state: "active", renews_at: null, manage: "app_store" }
    ]
  };
}

function json(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "Content-Type": "application/json" }
  });
}

function fixture() {
  let identity: AppleBillingIdentity = {
    apiOrigin: "https://opensecret.example.test",
    principalId: "account-a",
    revision: 1
  };
  let listener: ((transaction: SignedStoreKitTransaction) => void) | undefined;
  let listed = [transaction];
  let purchaseResult: StoreKitPurchaseResult = { status: "success", transaction };
  let mints = 0;
  let syncs = 0;
  let removals = 0;
  const finished: string[] = [];
  const purchases: { productId: string; token: string }[] = [];
  const notifications: AppleTransactionResponse[] = [];
  const errors: unknown[] = [];
  const calls: { url: string; init: RequestInit }[] = [];
  const bridge: StoreKitBridge = {
    getProducts: async () => [],
    purchase: async (productId, token) => {
      purchases.push({ productId, token });
      return purchaseResult;
    },
    getSignedTransactions: async () => ({
      transactions: listed,
      unfinishedTransactionIds: listed.map((tx) => tx.transactionId)
    }),
    finishTransaction: async (id) => {
      finished.push(id);
    },
    sync: async () => {
      syncs++;
    },
    getStorefront: async () => "USA",
    manageSubscriptions: async () => {},
    onStorefront: async () => () => {},
    onTransaction: async (callback) => {
      listener = callback;
      return () => {
        removals++;
      };
    }
  };
  const options: AppleBillingSessionOptions = {
    userId: "account-a",
    openSecretApiUrl: identity.apiOrigin,
    billingUrl: "https://billing.example.test",
    readIdentity: () => identity,
    auth: {
      generateThirdPartyToken: async (audience) => {
        expect(audience).toBe("https://billing.example.test");
        return { token: `test-jwt-${++mints}` };
      }
    },
    bridge,
    fetch: async (input, init) => {
      const url = String(input);
      calls.push({ url, init: init! });
      return url.endsWith("/account-token")
        ? json({ app_account_token: accountToken })
        : json(response());
    },
    onAcknowledged: (ack) => {
      notifications.push(ack);
    },
    onListenerError: (error) => {
      errors.push(error);
    }
  };
  return {
    options,
    bridge,
    finished,
    purchases,
    notifications,
    errors,
    calls,
    get mints() {
      return mints;
    },
    get syncs() {
      return syncs;
    },
    get removals() {
      return removals;
    },
    emit(tx = transaction) {
      listener?.(tx);
    },
    identity(value: AppleBillingIdentity) {
      identity = value;
    },
    advance(principalId: string | null = "account-a") {
      identity = { ...identity, principalId, revision: identity.revision + 1 };
    },
    list(value: SignedStoreKitTransaction[]) {
      listed = value;
    },
    result(value: StoreKitPurchaseResult) {
      purchaseResult = value;
    },
    session(overrides: Partial<AppleBillingSessionOptions> = {}) {
      return new AppleBillingSession({ ...options, ...overrides });
    }
  };
}

describe("authenticated Apple billing session", () => {
  test("listener completion is published only after every revision acknowledges and finish succeeds", async () => {
    const f = fixture();
    const completed = deferred<void>();
    const finishing = deferred<void>();
    const finishStarted = deferred<void>();
    let recovered = false;
    let rejected = true;
    const session = f.session({
      fetch: async () => (rejected ? json({}, 409) : json(response())),
      bridge: {
        ...f.bridge,
        finishTransaction: async (id) => {
          finishStarted.resolve();
          await finishing.promise;
          await f.bridge.finishTransaction(id);
        }
      },
      onListenerRecovered: () => {
        recovered = true;
        completed.resolve();
      }
    });
    await expect(session.start()).rejects.toBeInstanceOf(StoreKitRecoveryError);
    expect(recovered).toBe(false);
    rejected = false;
    f.emit({ ...transaction, jws: "fresh-listener-state" });
    await finishStarted.promise;
    expect(recovered).toBe(false);
    finishing.resolve();
    await completed.promise;
    expect(f.finished).toEqual([transaction.transactionId]);
    session.dispose();
  });

  for (const status of [400, 409]) {
    test(`${status} recovery waits for changed evidence or explicit restore across SDK refresh`, async () => {
      const f = fixture();
      const retryPolicy = new AppleBillingRetryPolicy();
      const submissions: string[] = [];
      let rejected = true;
      const fetch: AppleBillingSessionOptions["fetch"] = async (_input, init) => {
        const jws = JSON.parse(init!.body as string).signed_transaction as string;
        submissions.push(jws);
        return rejected ? json({}, status) : json(response());
      };
      const session = f.session({ retryPolicy, fetch });
      await expect(session.start()).rejects.toBeInstanceOf(StoreKitRecoveryError);
      await expect(session.start()).rejects.toBeInstanceOf(StoreKitRecoveryError);
      f.emit();
      await expect(session.recover()).rejects.toBeInstanceOf(StoreKitRecoveryError);
      expect(submissions).toEqual([transaction.jws]);
      expect(f.finished).toEqual([]);

      session.dispose();
      f.advance();
      const refreshed = f.session({ retryPolicy, fetch });
      await expect(refreshed.start()).rejects.toBeInstanceOf(StoreKitRecoveryError);
      expect(submissions).toEqual([transaction.jws]);

      f.list([{ ...transaction, jws: "new-signed-state" }]);
      await expect(refreshed.recover()).rejects.toBeInstanceOf(StoreKitRecoveryError);
      expect(submissions).toEqual([transaction.jws, transaction.jws, "new-signed-state"]);
      // Repeated enumeration must not unlock an already-observed revision.
      await expect(refreshed.recover()).rejects.toBeInstanceOf(StoreKitRecoveryError);
      expect(submissions).toHaveLength(3);
      rejected = false; // e.g. support resolves the ownership/configuration issue.
      await expect(refreshed.restore()).resolves.toEqual([response()]);
      expect(f.syncs).toBe(1);
      expect(f.finished).toEqual([transaction.transactionId]);
      expect(submissions).toHaveLength(5);
      refreshed.dispose();
    });
  }

  test("a conflicted transaction does not resubmit while an independent 503 recovers", async () => {
    const f = fixture();
    const other = { ...transaction, transactionId: "456", jws: "other-signed-state" };
    f.list([transaction, other]);
    const submitted: string[] = [];
    let unavailable = true;
    const session = f.session({
      fetch: async (_input, init) => {
        const jws = JSON.parse(init!.body as string).signed_transaction as string;
        submitted.push(jws);
        if (jws === transaction.jws) return json({}, 409);
        return unavailable ? json({}, 503) : json(response(other.transactionId));
      }
    });
    await expect(session.start()).rejects.toBeInstanceOf(StoreKitRecoveryError);
    unavailable = false;
    await expect(session.recover()).rejects.toBeInstanceOf(StoreKitRecoveryError);
    expect(submitted).toEqual([transaction.jws, other.jws, other.jws]);
    expect(f.finished).toEqual([other.transactionId]);
    session.dispose();
  });

  test("new signed state retries retained conflicts without allowing an unacknowledged finish", async () => {
    const f = fixture();
    const submitted: string[] = [];
    let oldStatus = 409;
    const session = f.session({
      fetch: async (_input, init) => {
        const jws = JSON.parse(init!.body as string).signed_transaction as string;
        submitted.push(jws);
        return jws === transaction.jws && oldStatus !== 200
          ? json({}, oldStatus)
          : json(response());
      }
    });
    await expect(session.start()).rejects.toBeInstanceOf(StoreKitRecoveryError);
    f.list([{ ...transaction, jws: "updated-state" }]);
    await expect(session.recover()).rejects.toBeInstanceOf(StoreKitRecoveryError);
    expect(submitted).toEqual([transaction.jws, transaction.jws, "updated-state"]);
    expect(f.finished).toEqual([]);
    oldStatus = 200;
    await session.restore();
    expect(submitted).toEqual([transaction.jws, transaction.jws, "updated-state", transaction.jws]);
    expect(f.finished).toEqual([transaction.transactionId]);
    session.dispose();
  });

  test("429 remains retryable and a late old-owner conflict cannot poison the current policy", async () => {
    const f = fixture();
    const retryPolicy = new AppleBillingRetryPolicy();
    let attempts = 0;
    const stale = deferred<Response>();
    const started = deferred<void>();
    const session = f.session({
      retryPolicy,
      fetch: async () => {
        if (++attempts === 1) return json({}, 429);
        started.resolve();
        return stale.promise;
      }
    });
    await expect(session.start()).rejects.toBeInstanceOf(StoreKitRecoveryError);
    const pending = session.recover().catch((error: unknown) => error);
    await started.promise;
    expect(attempts).toBe(2);
    session.dispose();
    f.advance();
    stale.resolve(json({}, 409));
    expect(await pending).toBeInstanceOf(Error);
    const refreshed = f.session({ retryPolicy });
    await refreshed.start();
    expect(f.finished).toEqual([transaction.transactionId]);
    refreshed.dispose();
  });

  test("purchase passes the server account token and finishes after a provider-independent acknowledgement", async () => {
    const f = fixture();
    const session = f.session();
    const result = await session.purchase(transaction.productId);
    expect(result).toEqual({ status: "success", acknowledgement: response() });
    expect(f.purchases).toEqual([{ productId: transaction.productId, token: accountToken }]);
    expect(f.finished).toEqual([transaction.transactionId]);
    expect(f.notifications).toEqual([response()]);
    expect(f.mints).toBe(1);
    expect(await session.getAccountToken()).toBe(accountToken);
    expect(f.calls).toHaveLength(2);
    expect(f.calls[1].init.headers).toMatchObject({ Authorization: "Bearer test-jwt-1" });
    expect(JSON.parse(f.calls[1].init.body as string)).toEqual({
      signed_transaction: transaction.jws
    });
    session.dispose();
  });

  for (const status of ["pending", "cancelled"] as const) {
    test(`${status} does not submit or finish a transaction`, async () => {
      const f = fixture();
      f.result({ status });
      const session = f.session();
      expect(await session.purchase(transaction.productId)).toEqual({ status });
      expect(f.calls).toHaveLength(1);
      expect(f.finished).toEqual([]);
      expect(f.notifications).toEqual([]);
      session.dispose();
    });
  }

  test("coalesces concurrent token minting and refreshes a rejected JWT only once", async () => {
    const f = fixture();
    const other = { ...transaction, transactionId: "0", jws: "second-test-transaction" };
    const mint = deferred<{ token: string }>();
    const mintStarted = deferred<void>();
    let mints = 0;
    const used: string[] = [];
    const session = f.session({
      auth: {
        generateThirdPartyToken: async () => {
          mints++;
          if (mints === 1) {
            mintStarted.resolve();
            return mint.promise;
          }
          return { token: "fresh-jwt" };
        }
      },
      fetch: async (_input, init) => {
        const token = new Headers(init?.headers).get("Authorization")!;
        used.push(token);
        if (token === "Bearer stale-jwt") return json({ error: "ignored" }, 401);
        const body = JSON.parse(init?.body as string);
        return json(
          response(
            body.signed_transaction === other.jws ? other.transactionId : transaction.transactionId
          )
        );
      }
    });
    const first = session.acknowledge(transaction);
    const second = session.acknowledge(other);
    await mintStarted.promise;
    expect(mints).toBe(1);
    mint.resolve({ token: "stale-jwt" });
    await Promise.all([first, second]);
    expect(mints).toBe(2);
    expect(used.filter((value) => value === "Bearer fresh-jwt")).toHaveLength(2);
    expect([...f.finished].sort()).toEqual(["0", transaction.transactionId]);
    session.dispose();
  });

  test("a second 401 is returned without another refresh or finish", async () => {
    const f = fixture();
    let requests = 0;
    const session = f.session({
      fetch: async () => {
        requests++;
        return json({}, 401);
      }
    });
    await expect(session.acknowledge(transaction)).rejects.toBeInstanceOf(AppleBillingApiError);
    expect(requests).toBe(2);
    expect(f.mints).toBe(2);
    expect(f.finished).toEqual([]);
    session.dispose();
  });

  test("unobserved A-to-B-to-A during token minting cannot issue a billing request", async () => {
    const f = fixture();
    const mint = deferred<{ token: string }>();
    const started = deferred<void>();
    const session = f.session({
      auth: {
        generateThirdPartyToken: () => {
          started.resolve();
          return mint.promise;
        }
      }
    });
    const pending = session.getAccountToken();
    await started.promise;
    f.advance("account-b");
    f.advance("account-a");
    mint.resolve({ token: "old-account-jwt" });
    await expect(pending).rejects.toBeInstanceOf(AppleBillingSessionChangedError);
    expect(f.calls).toEqual([]);
    expect(() => session.assertCurrent()).toThrow("apple_billing_session_changed");
  });

  test("same-user credential refresh revokes an old response and a fresh owner can recover", async () => {
    const f = fixture();
    const held = deferred<Response>();
    const started = deferred<AbortSignal>();
    const old = f.session({
      fetch: (_input, init) => {
        started.resolve(init!.signal!);
        return held.promise;
      }
    });
    const pending = old.acknowledge(transaction);
    const signal = await started.promise;
    f.advance();
    held.resolve(json(response()));
    await expect(pending).rejects.toBeInstanceOf(AppleBillingSessionChangedError);
    // The HTTP response completed before the session noticed the SDK refresh.
    // Its deadline/parent listener is already released; the old session itself
    // remains revoked even though that completed transport is detached.
    expect(signal.aborted).toBe(false);
    expect(() => old.assertCurrent()).toThrow(AppleBillingSessionChangedError);
    expect(f.finished).toEqual([]);
    expect(f.notifications).toEqual([]);
    const current = f.session();
    expect(await current.recover()).toEqual([response()]);
    expect(f.finished).toEqual([transaction.transactionId]);
    current.dispose();
  });

  test("synchronous revocation from an acknowledgement observer fences the native finish", async () => {
    const f = fixture();
    const session = f.session({ onAcknowledged: () => f.advance("account-b") });
    await expect(session.acknowledge(transaction)).rejects.toBeInstanceOf(
      AppleBillingSessionChangedError
    );
    expect(f.finished).toEqual([]);
  });

  test("disposal aborts a pending HTTP request even if fetch ignores abort", async () => {
    const f = fixture();
    const held = deferred<Response>();
    const started = deferred<AbortSignal>();
    const session = f.session({
      fetch: (_input, init) => {
        started.resolve(init!.signal!);
        return held.promise;
      }
    });
    const pending = session.acknowledge(transaction);
    const signal = await started.promise;
    session.dispose();
    expect(signal.aborted).toBe(true);
    held.resolve(json(response()));
    await expect(pending).rejects.toBeInstanceOf(AppleBillingSessionChangedError);
    expect(f.finished).toEqual([]);
    expect(f.notifications).toEqual([]);
  });

  test("late listener registration is unregistered after disposal without enumeration", async () => {
    const f = fixture();
    const registered = deferred<() => void>();
    const started = deferred<void>();
    let removed = 0;
    const session = f.session({
      bridge: {
        ...f.bridge,
        onTransaction: () => {
          started.resolve();
          return registered.promise;
        }
      }
    });
    const pending = session.start();
    await started.promise;
    session.dispose();
    registered.resolve(() => {
      removed++;
    });
    await expect(pending).rejects.toBeInstanceOf(AppleBillingSessionChangedError);
    expect(removed).toBe(1);
    expect(f.calls).toEqual([]);
  });

  test("listener revisions observed during submission cannot finish before every acknowledgement", async () => {
    const f = fixture();
    f.list([]);
    const first = deferred<Response>();
    const firstStarted = deferred<void>();
    const second = deferred<Response>();
    const secondStarted = deferred<void>();
    const finished = deferred<void>();
    const revised = { ...transaction, jws: "newer-test-transaction" };
    const session = f.session({
      bridge: {
        ...f.bridge,
        finishTransaction: async (id) => {
          await f.bridge.finishTransaction(id);
          finished.resolve();
        }
      },
      fetch: (_input, init) => {
        if (JSON.parse(init?.body as string).signed_transaction === transaction.jws) {
          firstStarted.resolve();
          return first.promise;
        }
        secondStarted.resolve();
        return second.promise;
      }
    });
    await session.start();
    f.emit();
    await firstStarted.promise;
    f.emit(revised);
    first.resolve(json(response()));
    await secondStarted.promise;
    expect(f.finished).toEqual([]);
    second.resolve(json(response()));
    await finished.promise;
    expect(f.finished).toEqual([transaction.transactionId]);
    session.dispose();
    f.emit();
    expect(f.removals).toBe(1);
  });

  test("partial recovery retains typed ownership failures and completed status; restore alone calls sync", async () => {
    const f = fixture();
    const other = { ...transaction, transactionId: "7", jws: "other-owner-test-transaction" };
    f.list([transaction, other]);
    const session = f.session({
      fetch: async (_input, init) =>
        JSON.parse(init?.body as string).signed_transaction === other.jws
          ? json({}, 409)
          : json(response())
    });
    const error: unknown = await session.recover().catch((failure: unknown) => failure);
    expect(error).toBeInstanceOf(StoreKitRecoveryError);
    if (!(error instanceof StoreKitRecoveryError)) throw new Error("expected partial failure");
    expect(error.acknowledgements).toEqual([response()]);
    expect(error.failures).toHaveLength(1);
    expect(error.failures[0].transactionId).toBe(other.transactionId);
    expect(error.failures[0].error).toMatchObject({ code: "conflict", status: 409 });
    expect(f.finished).toEqual([transaction.transactionId]);
    expect(f.syncs).toBe(0);
    await expect(session.restore()).rejects.toBeInstanceOf(StoreKitRecoveryError);
    expect(f.syncs).toBe(1);
    session.dispose();
  });

  test("the shipped JS StoreKit bridge carries the account token and exact ID over Tauri IPC", async () => {
    // Other suites intentionally replace @tauri-apps/api/core process-wide.
    // A fresh pinned Bun process exercises the shipped bridge and core module.
    if (process.env.APPLE_BILLING_ISOLATED_IPC_TEST !== "1") {
      const isolated = spawnSync(
        process.execPath,
        [
          "--no-env-file",
          "test",
          import.meta.path,
          "--test-name-pattern",
          "the shipped JS StoreKit bridge"
        ],
        {
          env: { ...process.env, APPLE_BILLING_ISOLATED_IPC_TEST: "1" },
          encoding: "utf8",
          timeout: 30_000
        }
      );
      expect(isolated.error).toBeUndefined();
      expect(isolated.status).toBe(0);
      return;
    }
    const prior = Object.getOwnPropertyDescriptor(globalThis, "window");
    const commands: { command: string; args: unknown }[] = [];
    Object.defineProperty(globalThis, "window", {
      configurable: true,
      value: {
        __TAURI_INTERNALS__: {
          invoke: async (command: string, args: unknown) => {
            commands.push({ command, args });
            if (command === "plugin:iap|purchase") return { status: "success", transaction };
            if (command === "plugin:iap|finish_transaction") return undefined;
            throw new Error("unexpected IPC command");
          }
        }
      }
    });
    const f = fixture();
    const session = f.session({ bridge: storeKit });
    try {
      expect((await session.purchase(transaction.productId)).status).toBe("success");
      expect(commands).toEqual([
        {
          command: "plugin:iap|purchase",
          args: { productId: transaction.productId, appAccountToken: accountToken }
        },
        {
          command: "plugin:iap|finish_transaction",
          args: { transactionId: transaction.transactionId }
        }
      ]);
    } finally {
      session.dispose();
      if (prior) Object.defineProperty(globalThis, "window", prior);
      else Reflect.deleteProperty(globalThis, "window");
    }
  });
});
