import { describe, expect, test } from "bun:test";
import {
  StoreKitRecovery,
  StoreKitRecoveryError,
  type SignedStoreKitTransaction,
  type StoreKitAcknowledgement
} from "./storeKitService";

const transaction: SignedStoreKitTransaction = {
  transactionId: "9007199254740993",
  originalTransactionId: "9007199254740993",
  productId: "fixture.pro",
  jws: "fixture.signature.must.never.be.logged"
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

function controlledSubmission() {
  const started = deferred<void>();
  const response = deferred<StoreKitAcknowledgement>();
  return {
    started: started.promise,
    resolve: response.resolve,
    reject: response.reject,
    submit() {
      started.resolve();
      return response.promise;
    }
  };
}

function fixture() {
  const finished: string[] = [];
  let syncs = 0;
  return {
    finished,
    get syncs() {
      return syncs;
    },
    bridge: {
      async finishTransaction(id: string) {
        finished.push(id);
      },
      async getSignedTransactions() {
        return {
          transactions: [transaction],
          unfinishedTransactionIds: [transaction.transactionId]
        };
      },
      async sync() {
        syncs++;
      }
    }
  };
}

describe("StoreKit acknowledgement ordering", () => {
  test("retains an unfinished purchase until billing acknowledges its exact string ID", async () => {
    const f = fixture();
    const ack = deferred<{ acknowledged_transaction_id: string; payment_provider: string }>();
    const recovery = new StoreKitRecovery(f.bridge, () => ack.promise);
    const pending = recovery.acknowledge(transaction);
    await Promise.resolve();
    expect(f.finished).toEqual([]);
    ack.resolve({
      acknowledged_transaction_id: transaction.transactionId,
      payment_provider: "stripe"
    });
    await pending;
    expect(f.finished).toEqual([transaction.transactionId]);
  });

  test("does not finish on a wrong acknowledgement or server failure", async () => {
    for (const submit of [
      async () => ({ acknowledged_transaction_id: "other" }),
      async () => {
        throw new Error("503");
      }
    ]) {
      const f = fixture();
      const recovery = new StoreKitRecovery(f.bridge, submit);
      await expect(recovery.acknowledge(transaction)).rejects.toThrow();
      expect(f.finished).toEqual([]);
    }
  });

  test("coalesces listener and purchase delivery while preserving later signed snapshots", async () => {
    const f = fixture();
    const submitted: string[] = [];
    const ack = deferred<{ acknowledged_transaction_id: string }>();
    const recovery = new StoreKitRecovery(f.bridge, async (jws) => {
      submitted.push(jws);
      return ack.promise;
    });
    const first = recovery.acknowledge(transaction);
    expect(recovery.acknowledge(transaction)).toBe(first);
    ack.resolve({ acknowledged_transaction_id: transaction.transactionId });
    await first;
    await recovery.acknowledge({ ...transaction, jws: "newer.signed.snapshot" });
    expect(submitted).toEqual([transaction.jws, "newer.signed.snapshot"]);
  });

  test("late acknowledgement after account disposal leaves recovery to the owner", async () => {
    const f = fixture();
    const ack = deferred<{ acknowledged_transaction_id: string }>();
    const recovery = new StoreKitRecovery(f.bridge, () => ack.promise);
    const pending = recovery.acknowledge(transaction);
    recovery.dispose();
    ack.resolve({ acknowledged_transaction_id: transaction.transactionId });
    await expect(pending).rejects.toThrow("storekit_session_changed");
    expect(f.finished).toEqual([]);
  });

  test("session disposal during listing prevents submission", async () => {
    const f = fixture();
    const listed = deferred<{
      transactions: SignedStoreKitTransaction[];
      unfinishedTransactionIds: string[];
    }>();
    let submissions = 0;
    const recovery = new StoreKitRecovery(
      { ...f.bridge, getSignedTransactions: () => listed.promise },
      async () => {
        submissions++;
        return { acknowledged_transaction_id: transaction.transactionId };
      }
    );
    const pending = recovery.recover();
    recovery.dispose();
    listed.resolve({
      transactions: [transaction],
      unfinishedTransactionIds: [transaction.transactionId]
    });
    await expect(pending).rejects.toThrow("storekit_session_changed");
    expect(submissions).toBe(0);
  });

  test("a failed finish can recover later and silent recovery never calls sync", async () => {
    const f = fixture();
    let attempts = 0;
    const recovery = new StoreKitRecovery(
      {
        ...f.bridge,
        async finishTransaction(id) {
          if (++attempts === 1) throw new Error("native unavailable");
          await f.bridge.finishTransaction(id);
        }
      },
      async () => ({ acknowledged_transaction_id: transaction.transactionId })
    );
    await expect(recovery.recover()).rejects.toThrow("storekit_recovery_incomplete");
    await recovery.recover();
    expect(f.finished).toEqual([transaction.transactionId]);
    expect(f.syncs).toBe(0);
    await recovery.restore();
    expect(f.syncs).toBe(1);
  });

  test("waits for both observed revisions and coalesces duplicate deliveries before finishing once", async () => {
    const f = fixture();
    const older = controlledSubmission();
    const newer = controlledSubmission();
    const revised = { ...transaction, jws: "newer.signed.snapshot" };
    const submitted: string[] = [];
    const recovery = new StoreKitRecovery(f.bridge, (jws) => {
      submitted.push(jws);
      return jws === transaction.jws ? older.submit() : newer.submit();
    });
    const first = recovery.acknowledge(transaction);
    await older.started;
    expect(recovery.acknowledge(transaction)).toBe(first);
    const second = recovery.acknowledge(revised);
    expect(recovery.acknowledge(revised)).toBe(second);
    expect(submitted).toEqual([transaction.jws]);

    older.resolve({ acknowledged_transaction_id: transaction.transactionId });
    await newer.started;
    expect(f.finished).toEqual([]);
    newer.resolve({ acknowledged_transaction_id: transaction.transactionId });
    await Promise.all([first, second]);

    expect(submitted).toEqual([transaction.jws, revised.jws]);
    expect(f.finished).toEqual([transaction.transactionId]);
  });

  for (const retryMode of ["acknowledge", "recover", "recover-empty"] as const) {
    test(`retains a failed newer revision and retries it through ${retryMode} before finishing`, async () => {
      const f = fixture();
      const older = controlledSubmission();
      const newer = controlledSubmission();
      const retry = controlledSubmission();
      const revised = { ...transaction, jws: "revoked.signed.snapshot" };
      const submitted: string[] = [];
      let newerAttempts = 0;
      const bridge = {
        ...f.bridge,
        getSignedTransactions:
          retryMode === "recover-empty"
            ? async () => ({ transactions: [], unfinishedTransactionIds: [] })
            : f.bridge.getSignedTransactions
      };
      const recovery = new StoreKitRecovery(bridge, (jws) => {
        submitted.push(jws);
        if (jws === transaction.jws) return older.submit();
        return ++newerAttempts === 1 ? newer.submit() : retry.submit();
      });
      const first = recovery.acknowledge(transaction);
      await older.started;
      const second = recovery.acknowledge(revised);
      const settled = Promise.allSettled([first, second]);
      older.resolve({ acknowledged_transaction_id: transaction.transactionId });
      await newer.started;
      expect(f.finished).toEqual([]);
      newer.reject(new Error("billing unavailable"));
      expect((await settled).map((result) => result.status)).toEqual(["rejected", "rejected"]);
      expect(f.finished).toEqual([]);

      // Native recovery lists the original snapshot or nothing: the failed
      // revision must survive in memory rather than depend on being listed.
      const retried =
        retryMode === "acknowledge" ? recovery.acknowledge(revised) : recovery.recover();
      await retry.started;
      expect(f.finished).toEqual([]);
      retry.resolve({ acknowledged_transaction_id: transaction.transactionId });
      await retried;
      expect(submitted).toEqual([transaction.jws, revised.jws, revised.jws]);
      expect(f.finished).toEqual([transaction.transactionId]);
    });
  }

  test("a revision arriving during native finish waits for its own acknowledgement and finish", async () => {
    const f = fixture();
    const finishStarted = deferred<void>();
    const finishCompleted = deferred<void>();
    const newer = controlledSubmission();
    const revised = { ...transaction, jws: "newer.signed.snapshot" };
    const submitted: string[] = [];
    const finishInvocations: string[] = [];
    const recovery = new StoreKitRecovery(
      {
        ...f.bridge,
        async finishTransaction(id) {
          finishInvocations.push(id);
          if (finishInvocations.length === 1) {
            finishStarted.resolve();
            await finishCompleted.promise;
          }
          await f.bridge.finishTransaction(id);
        }
      },
      (jws) => {
        submitted.push(jws);
        return jws === transaction.jws
          ? Promise.resolve({ acknowledged_transaction_id: transaction.transactionId })
          : newer.submit();
      }
    );
    const first = recovery.acknowledge(transaction);
    await finishStarted.promise;
    const second = recovery.acknowledge(revised);
    expect(submitted).toEqual([transaction.jws]);
    expect(finishInvocations).toEqual([transaction.transactionId]);
    expect(f.finished).toEqual([]);

    // The native call already began, so its completion cannot be withdrawn.
    finishCompleted.resolve();
    await first;
    await newer.started;
    expect(f.finished).toEqual([transaction.transactionId]);
    expect(finishInvocations).toEqual([transaction.transactionId]);
    newer.resolve({ acknowledged_transaction_id: transaction.transactionId });
    await second;
    expect(submitted).toEqual([transaction.jws, revised.jws]);
    expect(finishInvocations).toEqual([transaction.transactionId, transaction.transactionId]);
    expect(f.finished).toEqual([transaction.transactionId, transaction.transactionId]);
  });

  test("disposal while a newer revision awaits acknowledgement prevents finishing the batch", async () => {
    const f = fixture();
    const older = controlledSubmission();
    const newer = controlledSubmission();
    const recovery = new StoreKitRecovery(f.bridge, (jws) =>
      jws === transaction.jws ? older.submit() : newer.submit()
    );
    const first = recovery.acknowledge(transaction);
    await older.started;
    const second = recovery.acknowledge({ ...transaction, jws: "newer.signed.snapshot" });
    const settled = Promise.allSettled([first, second]);
    older.resolve({ acknowledged_transaction_id: transaction.transactionId });
    await newer.started;
    recovery.dispose();
    newer.resolve({ acknowledged_transaction_id: transaction.transactionId });

    for (const result of await settled) {
      expect(result.status).toBe("rejected");
      if (result.status === "rejected") {
        expect(result.reason).toBeInstanceOf(Error);
        expect(result.reason.message).toBe("storekit_session_changed");
      }
    }
    expect(f.finished).toEqual([]);
  });

  test("a pending transaction does not block a different transaction ID", async () => {
    const f = fixture();
    const pending = controlledSubmission();
    const other = {
      ...transaction,
      transactionId: "9007199254740995",
      originalTransactionId: "9007199254740995",
      jws: "other.signed.snapshot"
    };
    const recovery = new StoreKitRecovery(f.bridge, (jws) =>
      jws === transaction.jws
        ? pending.submit()
        : Promise.resolve({ acknowledged_transaction_id: other.transactionId })
    );
    const first = recovery.acknowledge(transaction);
    await pending.started;
    await recovery.acknowledge(other);
    expect(f.finished).toEqual([other.transactionId]);
    pending.resolve({ acknowledged_transaction_id: transaction.transactionId });
    await first;
    expect(f.finished).toEqual([other.transactionId, transaction.transactionId]);
  });

  test("partial recovery reports completed acknowledgements in a typed error", async () => {
    const f = fixture();
    const failedTransaction = {
      ...transaction,
      transactionId: "9007199254740995",
      jws: "wrong.owner.signed.snapshot"
    };
    const acknowledgement = {
      acknowledged_transaction_id: transaction.transactionId,
      payment_provider: "stripe"
    };
    const recovery = new StoreKitRecovery(
      {
        ...f.bridge,
        async getSignedTransactions() {
          return {
            transactions: [transaction, failedTransaction],
            unfinishedTransactionIds: [transaction.transactionId, failedTransaction.transactionId]
          };
        }
      },
      async (jws) => {
        if (jws === failedTransaction.jws) throw new Error("ownership conflict");
        return acknowledgement;
      }
    );

    const result = await recovery.recover().catch((error: unknown) => error);
    expect(result).toBeInstanceOf(StoreKitRecoveryError);
    if (!(result instanceof StoreKitRecoveryError)) throw new Error("expected partial recovery");
    expect(result.message).toBe("storekit_recovery_incomplete");
    expect(result.acknowledgements).toEqual([acknowledgement]);
    expect(f.finished).toEqual([transaction.transactionId]);
  });
});
