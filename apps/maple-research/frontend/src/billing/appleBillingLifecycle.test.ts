import { describe, expect, test } from "bun:test";
import {
  AppleBillingLifecycle,
  registerAppleBillingLifecycle,
  suspendAppleBillingForAccount
} from "./appleBillingLifecycle";
import {
  AppleBillingSessionChangedError,
  type AppleBillingIdentity,
  type AppleBillingPurchaseResult
} from "./appleBillingSession";
import { AppleBillingApiError, type AppleTransactionResponse } from "./appleBillingApi";
import { StoreKitRecoveryError } from "@/services/storeKitService";
import type { AppleBillingRetryPolicy } from "./appleBillingRetryPolicy";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

async function flush() {
  for (let n = 0; n < 12; n++) await Promise.resolve();
}

const acknowledgement: AppleTransactionResponse = {
  acknowledged_transaction_id: "123",
  payment_provider: "stripe",
  is_subscribed: true,
  product_id: "max",
  product_name: "Max",
  subscription_status: "active",
  current_period_end: null,
  can_chat: true,
  chats_remaining: null,
  total_tokens: null,
  used_tokens: null,
  usage_reset_date: null,
  ios_iap_enabled: false,
  subscriptions: [
    { provider: "apple", plan: "Pro", state: "active", renews_at: null, manage: "app_store" }
  ]
};

function fixture() {
  let identity: AppleBillingIdentity = {
    apiOrigin: "https://auth.example.test",
    revision: 1,
    principalId: "a"
  };
  let now = 0;
  const starts: ReturnType<typeof deferred<AppleTransactionResponse[]>>[] = [];
  const purchases: ReturnType<typeof deferred<AppleBillingPurchaseResult>>[] = [];
  const restores: ReturnType<typeof deferred<AppleTransactionResponse[]>>[] = [];
  const published: { status: AppleTransactionResponse; owner: string }[] = [];
  const sessions: {
    disposed: boolean;
    owner: string;
    ack: (status: AppleTransactionResponse) => void;
    fail: (error: unknown) => void;
    retryPolicy: AppleBillingRetryPolicy;
    recovered: () => void;
  }[] = [];
  const lifecycle = new AppleBillingLifecycle({
    readIdentity: () => identity,
    now: () => now,
    createSession: (owner, ack, fail, retryPolicy, recovered) => {
      const revision = identity.revision;
      const session = { disposed: false, owner, ack, fail, retryPolicy, recovered };
      sessions.push(session);
      return {
        assertCurrent: () => {
          if (
            session.disposed ||
            owner !== identity.principalId ||
            revision !== identity.revision
          ) {
            throw new AppleBillingSessionChangedError();
          }
        },
        dispose: () => {
          session.disposed = true;
        },
        start: () => {
          const d = deferred<AppleTransactionResponse[]>();
          starts.push(d);
          return d.promise;
        },
        purchase: () => {
          const d = deferred<AppleBillingPurchaseResult>();
          purchases.push(d);
          return d.promise;
        },
        restore: () => {
          const d = deferred<AppleTransactionResponse[]>();
          restores.push(d);
          return d.promise;
        }
      };
    },
    onAcknowledged: (status, owner) => published.push({ status, owner })
  });
  lifecycle.activate();
  return {
    lifecycle,
    starts,
    purchases,
    restores,
    published,
    sessions,
    identity: (user: string | null, revision: number) => {
      identity = { ...identity, principalId: user, revision };
    },
    advance: (ms: number) => {
      now += ms;
      lifecycle.tick();
    }
  };
}

describe("Apple billing app lifecycle", () => {
  test("listener completion reconciles all failures before clearing an old ownership message", async () => {
    const f = fixture();
    f.lifecycle.setUser("a");
    const conflict = new AppleBillingApiError("conflict", 409);
    f.starts[0].reject(conflict);
    await flush();
    f.sessions[0].ack(acknowledgement);
    expect(f.lifecycle.getSnapshot().recoveryError).toContain("another Maple account");
    f.sessions[0].recovered();
    await flush();
    expect(f.starts).toHaveLength(2);
    f.starts[1].reject(conflict); // Another ID still has an ownership conflict.
    await flush();
    expect(f.lifecycle.getSnapshot().recoveryError).toContain("another Maple account");
    f.sessions[0].recovered();
    await flush();
    f.starts[2].resolve([acknowledgement]);
    await flush();
    expect(f.lifecycle.getSnapshot().recoveryError).toBeNull();
    f.lifecycle.dispose();
  });

  test("listener completion waits for overlapping recovery and cannot wake a revoked owner", async () => {
    const f = fixture();
    f.lifecycle.setUser("a");
    f.sessions[0].recovered();
    await flush();
    expect(f.starts).toHaveLength(1);
    f.starts[0].reject(new AppleBillingApiError("conflict", 409));
    await flush();
    expect(f.starts).toHaveLength(2);
    f.sessions[0].recovered();
    f.lifecycle.dispose();
    f.starts[1].resolve([]);
    await flush();
    expect(f.starts).toHaveLength(2);
  });

  test("automatic recovery preserves terminal rejection across refresh; manual retry lifts it", async () => {
    const f = fixture();
    f.lifecycle.setUser("a");
    const policy = f.sessions[0].retryPolicy;
    const error = new AppleBillingApiError("conflict", 409);
    policy.observe("123", "signed-state");
    policy.failed("123", "signed-state", error);
    f.starts[0].reject(new StoreKitRecoveryError([], [{ transactionId: "123", error }]));
    await flush();
    f.advance(300_000);
    expect(f.starts).toHaveLength(1);
    const wake = f.lifecycle.recoverAutomatically();
    expect(() => policy.assertMaySubmit("123", "signed-state")).toThrow(error);
    f.starts[1].reject(error);
    await expect(wake).rejects.toBe(error);
    f.identity("a", 2);
    f.lifecycle.tick();
    expect(f.sessions[1].retryPolicy).toBe(policy);
    expect(() => policy.assertMaySubmit("123", "signed-state")).toThrow(error);
    f.starts[2].reject(error);
    await flush();
    const manual = f.lifecycle.retry();
    expect(() => policy.assertMaySubmit("123", "signed-state")).not.toThrow();
    f.starts[3].resolve([acknowledgement]);
    await manual;
    expect(f.lifecycle.getSnapshot().recoveryError).toBeNull();
    f.identity("b", 3);
    f.lifecycle.setUser("b");
    expect(f.sessions[2].retryPolicy).not.toBe(policy);
    f.starts[4].resolve([]);
    await flush();
    f.lifecycle.dispose();
  });

  test("nested mixed failures retry with backoff that focus events cannot bypass", async () => {
    const f = fixture();
    f.lifecycle.setUser("a");
    const conflict = new AppleBillingApiError("conflict", 409);
    const transient = new AppleBillingApiError("unavailable", 429);
    f.starts[0].reject(
      new StoreKitRecoveryError(
        [],
        [
          {
            transactionId: "123",
            error: new StoreKitRecoveryError(
              [],
              [
                { transactionId: "123", error: conflict },
                { transactionId: "123", error: transient }
              ]
            )
          }
        ]
      )
    );
    await flush();
    expect(f.lifecycle.getSnapshot().recoveryError).toContain("another Maple account");
    await f.lifecycle.recoverAutomatically();
    f.advance(14_999);
    await f.lifecycle.recoverAutomatically();
    expect(f.starts).toHaveLength(1);
    f.advance(1);
    expect(f.starts).toHaveLength(2);
    f.starts[1].reject(conflict);
    await flush();
    f.advance(300_000);
    expect(f.starts).toHaveLength(2);
    f.lifecycle.dispose();
  });

  test("does not create a session until both UI and SDK agree on the account", async () => {
    const f = fixture();
    f.lifecycle.setUser("b");
    expect(f.sessions).toHaveLength(0);
    f.identity("b", 2);
    f.lifecycle.tick();
    expect(f.sessions.map((s) => s.owner)).toEqual(["b"]);
    expect(f.starts).toHaveLength(1);
    f.starts[0].resolve([]);
    await flush();
    expect(f.lifecycle.getSnapshot()).toMatchObject({
      ready: true,
      busy: false,
      recoveryError: null
    });
    f.lifecycle.dispose();
  });

  test("refresh replaces the session, revokes the listener, and ignores its late status/error", async () => {
    const f = fixture();
    f.lifecycle.setUser("a");
    f.identity("a", 2);
    f.lifecycle.tick();
    expect(f.sessions).toHaveLength(2);
    expect(f.sessions[0].disposed).toBe(true);
    f.sessions[0].ack(acknowledgement);
    f.sessions[0].fail(new Error("SECRET_RESPONSE"));
    f.starts[0].reject(new Error("OLD_FAILURE"));
    await flush();
    expect(f.published).toHaveLength(0);
    expect(f.lifecycle.getSnapshot()).toMatchObject({
      ready: true,
      busy: true,
      recoveryError: null
    });
    f.sessions[1].ack(acknowledgement);
    expect(f.published).toEqual([{ status: acknowledgement, owner: "a" }]);
    f.starts[1].resolve([]);
    await flush();
    expect(f.lifecycle.getSnapshot().busy).toBe(false);
    f.lifecycle.dispose();
  });

  test("suspension precedes awaited logout and late recovery cannot finish publication", async () => {
    const f = fixture();
    const unregister = registerAppleBillingLifecycle(f.lifecycle);
    f.lifecycle.setUser("a");
    const release = suspendAppleBillingForAccount("a");
    expect(f.sessions[0].disposed).toBe(true);
    f.advance(60_000);
    expect(f.sessions).toHaveLength(1);
    f.sessions[0].ack(acknowledgement);
    f.starts[0].resolve([acknowledgement]);
    await flush();
    expect(f.published).toHaveLength(0);
    expect(f.lifecycle.getSnapshot().ready).toBe(false);
    // A failed logout may reattach only a fresh owner after releasing the fence.
    release();
    release();
    expect(f.sessions).toHaveLength(2);
    f.starts[1].resolve([]);
    await flush();
    unregister();
    f.lifecycle.dispose();
  });

  test("retained deletion suspension for A does not block B or revive A", async () => {
    const f = fixture();
    f.lifecycle.setUser("a");
    f.lifecycle.suspend("a");
    f.identity("b", 2);
    f.lifecycle.setUser("b");
    expect(f.sessions.map((s) => s.owner)).toEqual(["a", "b"]);
    f.starts[0].resolve([]);
    f.starts[1].resolve([]);
    await flush();
    f.identity("a", 3);
    f.lifecycle.setUser("a");
    expect(f.sessions).toHaveLength(2);
    expect(f.lifecycle.getSnapshot().ready).toBe(false);
    f.lifecycle.dispose();
  });

  test("failed recovery has bounded backoff, foreground retry coalesces, errors are sanitized", async () => {
    const f = fixture();
    f.lifecycle.setUser("a");
    f.starts[0].reject(new Error("PRIVATE_JWS_RESPONSE"));
    await flush();
    expect(f.lifecycle.getSnapshot().recoveryError).not.toContain("PRIVATE");
    f.advance(14_999);
    expect(f.starts).toHaveLength(1);
    f.advance(1);
    expect(f.starts).toHaveLength(2);
    const first = f.lifecycle.retry();
    const second = f.lifecycle.retry();
    expect(f.starts).toHaveLength(2);
    f.starts[1].resolve([]);
    await Promise.all([first, second]);
    expect(f.lifecycle.getSnapshot().recoveryError).toBeNull();
    f.advance(300_000);
    expect(f.starts).toHaveLength(2);
    f.lifecycle.dispose();
  });

  test("a successful purchase does not discard another transaction's pending recovery", async () => {
    const f = fixture();
    f.lifecycle.setUser("a");
    f.starts[0].reject(new Error("offline"));
    await flush();
    const purchase = f.lifecycle.purchase("pro");
    f.purchases[0].resolve({ status: "cancelled" });
    await expect(purchase).resolves.toEqual({ status: "cancelled" });
    expect(f.lifecycle.getSnapshot().recoveryError).not.toBeNull();
    f.advance(15_000);
    expect(f.starts).toHaveLength(2);
    f.starts[1].resolve([]);
    await flush();
    f.lifecycle.dispose();
  });

  test("purchase results cannot cross accounts and duplicate clicks do not launch a second sheet", async () => {
    const f = fixture();
    f.lifecycle.setUser("a");
    f.starts[0].resolve([]);
    await flush();
    const purchase = f.lifecycle.purchase("pro");
    await expect(f.lifecycle.purchase("pro")).rejects.toThrow("apple_billing_busy");
    expect(f.purchases).toHaveLength(1);
    f.identity("b", 2);
    f.lifecycle.setUser("b");
    f.purchases[0].resolve({ status: "success", acknowledgement });
    await expect(purchase).rejects.toBeInstanceOf(AppleBillingSessionChangedError);
    expect(f.lifecycle.getSnapshot().busy).toBe(true);
    f.starts[1].resolve([]);
    await flush();
    f.lifecycle.dispose();
  });

  test("restore remains available with purchasing disabled and uses explicit StoreKit sync", async () => {
    const f = fixture();
    f.lifecycle.setUser("a");
    f.starts[0].resolve([]);
    await flush();
    const restore = f.lifecycle.restore();
    expect(f.restores).toHaveLength(1);
    f.sessions[0].ack(acknowledgement);
    f.restores[0].resolve([acknowledgement]);
    await expect(restore).resolves.toEqual([acknowledgement]);
    expect(f.published[0].status.payment_provider).toBe("stripe");
    f.lifecycle.dispose();
  });

  test("StrictMode setup-cleanup-setup uses a new session without reviving disposed work", async () => {
    const f = fixture();
    f.lifecycle.setUser("a");
    f.lifecycle.dispose();
    f.lifecycle.activate();
    f.lifecycle.setUser("a");
    expect(f.sessions).toHaveLength(2);
    expect(f.sessions[0].disposed).toBe(true);
    f.sessions[0].ack(acknowledgement);
    f.starts[0].resolve([]);
    f.starts[1].resolve([]);
    await flush();
    expect(f.published).toHaveLength(0);
    expect(f.lifecycle.getSnapshot().ready).toBe(true);
    f.lifecycle.dispose();
  });
});
