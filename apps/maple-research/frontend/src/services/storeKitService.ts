import { addPluginListener, invoke } from "@tauri-apps/api/core";

export interface StoreKitProduct {
  productId: string;
  title: string;
  description: string;
  formattedPrice: string;
  priceCurrencyCode: string;
  subscriptionPeriod: string | null;
}

export interface SignedStoreKitTransaction {
  transactionId: string;
  originalTransactionId: string;
  productId: string;
  jws: string;
}

export interface StoreKitTransactions {
  transactions: SignedStoreKitTransaction[];
  unfinishedTransactionIds: string[];
}

export type StoreKitPurchaseResult =
  | { status: "success"; transaction: SignedStoreKitTransaction }
  | { status: "pending" }
  | { status: "cancelled" };

export interface StoreKitBridge {
  getProducts(productIds: string[]): Promise<StoreKitProduct[]>;
  purchase(productId: string, appAccountToken: string): Promise<StoreKitPurchaseResult>;
  getSignedTransactions(): Promise<StoreKitTransactions>;
  finishTransaction(transactionId: string): Promise<void>;
  sync(): Promise<void>;
  getStorefront(): Promise<string | null>;
  manageSubscriptions(): Promise<void>;
  onTransaction(callback: (transaction: SignedStoreKitTransaction) => void): Promise<() => void>;
  onStorefront(callback: (countryCode: string | null) => void): Promise<() => void>;
}

/** iOS only. Callers initialize platform detection before selecting this adapter. */
export const storeKit: StoreKitBridge = {
  async getProducts(productIds) {
    const response = await invoke<{ products: StoreKitProduct[] }>("plugin:iap|get_products", {
      productIds
    });
    return response.products;
  },
  purchase: (productId, appAccountToken) =>
    invoke("plugin:iap|purchase", { productId, appAccountToken }),
  getSignedTransactions: () => invoke("plugin:iap|get_signed_transactions"),
  finishTransaction: (transactionId) => invoke("plugin:iap|finish_transaction", { transactionId }),
  sync: () => invoke("plugin:iap|sync"),
  async getStorefront() {
    const result = await invoke<{ countryCode: string | null }>("plugin:iap|get_storefront");
    return result.countryCode;
  },
  manageSubscriptions: () => invoke("plugin:iap|manage_subscriptions"),
  async onTransaction(callback) {
    const listener = await addPluginListener("iap", "transactionUpdated", callback);
    return () => listener.unregister();
  },
  async onStorefront(callback) {
    const listener = await addPluginListener<{ countryCode: string | null }>(
      "iap",
      "storefrontChanged",
      (event) => callback(event.countryCode)
    );
    return () => listener.unregister();
  }
};

export interface StoreKitAcknowledgement {
  acknowledged_transaction_id: string;
  // The selected entitlement can still be Stripe, a team, or another provider.
  payment_provider?: string | null;
}

export interface StoreKitRecoveryFailure {
  transactionId: string;
  error: unknown;
}

export class StoreKitRecoveryError<
  Acknowledgement extends StoreKitAcknowledgement = StoreKitAcknowledgement
> extends Error {
  readonly acknowledgements: readonly Acknowledgement[];
  readonly failures: readonly StoreKitRecoveryFailure[];

  constructor(
    acknowledgements: readonly Acknowledgement[],
    failures: readonly StoreKitRecoveryFailure[] = []
  ) {
    super("storekit_recovery_incomplete");
    this.name = "StoreKitRecoveryError";
    this.acknowledgements = [...acknowledgements];
    this.failures = [...failures];
  }
}

interface RevisionRequest<Acknowledgement> {
  promise: Promise<Acknowledgement>;
  resolve: (acknowledgement: Acknowledgement) => void;
  reject: (error: unknown) => void;
}

interface ObservedRevision<Acknowledgement> {
  jws: string;
  acknowledgement?: Acknowledgement;
  request?: RevisionRequest<Acknowledgement>;
}

interface TransactionRecovery<Acknowledgement> {
  revisions: Map<string, ObservedRevision<Acknowledgement>>;
  running: boolean;
}

/**
 * One instance belongs to one immutable authenticated account/session. Dispose
 * it on logout or account change; never replace its submit callback in place.
 * Recovery consults StoreKit and retains in-session failures; no JWS goes into web storage.
 */
export class StoreKitRecovery<
  Acknowledgement extends StoreKitAcknowledgement = StoreKitAcknowledgement
> {
  private active = true;
  private readonly transactions = new Map<string, TransactionRecovery<Acknowledgement>>();

  constructor(
    private readonly bridge: Pick<
      StoreKitBridge,
      "finishTransaction" | "getSignedTransactions" | "sync"
    >,
    private readonly submit: (
      signedTransaction: string,
      transactionId: string
    ) => Promise<Acknowledgement>
  ) {}

  dispose(): void {
    this.active = false;
    this.transactions.clear();
  }

  private assertActive(): void {
    if (!this.active) throw new Error("storekit_session_changed");
  }

  acknowledge(transaction: SignedStoreKitTransaction): Promise<Acknowledgement> {
    return this.acknowledgeRevision(transaction.transactionId, transaction.jws);
  }

  private acknowledgeRevision(transactionId: string, jws: string): Promise<Acknowledgement> {
    this.assertActive();
    let recovery = this.transactions.get(transactionId);
    if (!recovery) {
      recovery = { revisions: new Map(), running: false };
      this.transactions.set(transactionId, recovery);
    }
    let revision = recovery.revisions.get(jws);
    if (!revision) {
      revision = { jws };
      recovery.revisions.set(jws, revision);
    }
    if (revision.request) return revision.request.promise;
    let resolve!: RevisionRequest<Acknowledgement>["resolve"];
    let reject!: RevisionRequest<Acknowledgement>["reject"];
    const promise = new Promise<Acknowledgement>((accept, fail) => {
      resolve = accept;
      reject = fail;
    });
    revision.request = { promise, resolve, reject };
    if (!recovery.running) {
      recovery.running = true;
      void this.submitAndFinish(transactionId, recovery);
    }
    return promise;
  }

  private async submitAndFinish(
    transactionId: string,
    recovery: TransactionRecovery<Acknowledgement>
  ): Promise<void> {
    try {
      while (recovery.revisions.size > 0) {
        // Map iteration includes revisions added while a submission is pending.
        // Failed revisions remain in memory and must succeed on a later retry
        // before any acknowledged revision of this ID can cause a finish.
        for (const revision of recovery.revisions.values()) {
          this.assertActive();
          if (revision.acknowledgement) continue;
          const acknowledgement = await this.submit(revision.jws, transactionId);
          this.assertActive();
          if (acknowledgement.acknowledged_transaction_id !== transactionId) {
            throw new Error("storekit_acknowledgement_mismatch");
          }
          revision.acknowledgement = acknowledgement;
        }
        this.assertActive();
        const ready = [...recovery.revisions.values()];
        // All revisions observed before this invocation are acknowledged. A
        // revision arriving during the native call cannot retract that call;
        // it stays queued for its own submission and a subsequent finish.
        await this.bridge.finishTransaction(transactionId);
        this.assertActive();
        for (const revision of ready) {
          recovery.revisions.delete(revision.jws);
          revision.request?.resolve(revision.acknowledgement!);
        }
      }
      this.transactions.delete(transactionId);
    } catch (error) {
      for (const revision of recovery.revisions.values()) {
        revision.request?.reject(error);
        revision.request = undefined;
      }
    } finally {
      recovery.running = false;
    }
  }

  async recover(): Promise<Acknowledgement[]> {
    this.assertActive();
    const { transactions } = await this.bridge.getSignedTransactions();
    this.assertActive();
    const candidates = transactions.map(({ transactionId, jws }) => ({ transactionId, jws }));
    const listedIds = new Set(candidates.map(({ transactionId }) => transactionId));
    // Retry retained failures even when StoreKit omits the ID from this query.
    for (const [transactionId, recovery] of this.transactions) {
      if (!listedIds.has(transactionId)) {
        const revisions = [...recovery.revisions.keys()];
        const jws = revisions.at(-1);
        if (jws !== undefined) candidates.push({ transactionId, jws });
      }
    }
    // Register every observed revision before waiting. A slow or conflicting
    // transaction must not prevent another ID from submitting and finishing.
    const results = await Promise.allSettled(
      candidates.map(async ({ transactionId, jws }) => this.acknowledgeRevision(transactionId, jws))
    );
    this.assertActive();
    const acknowledgements = results.flatMap((result) =>
      result.status === "fulfilled" ? [result.value] : []
    );
    const failures = results.flatMap((result, index) =>
      result.status === "rejected"
        ? [{ transactionId: candidates[index].transactionId, error: result.reason as unknown }]
        : []
    );
    if (failures.length > 0) {
      throw new StoreKitRecoveryError(acknowledgements, failures);
    }
    return acknowledgements;
  }

  /** Only call from an explicit Restore action: sync may prompt for Apple login. */
  async restore(): Promise<Acknowledgement[]> {
    this.assertActive();
    await this.bridge.sync();
    this.assertActive();
    return this.recover();
  }
}
