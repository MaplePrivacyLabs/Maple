import { readNativeUserAuth, type OpenSecretContextType } from "@mapleai/sdk";
import { isIOS } from "@/utils/platform";
import { AppleBillingRetryPolicy } from "./appleBillingRetryPolicy";
import {
  storeKit,
  StoreKitRecovery,
  type SignedStoreKitTransaction,
  type StoreKitBridge
} from "@/services/storeKitService";
import {
  AppleBillingApiError,
  fetchAppleAccountToken,
  submitAppleTransaction,
  type AppleBillingRequestOptions,
  type AppleTransactionResponse
} from "./appleBillingApi";

/** Identity only. Never retain SDK credentials or its native cache root here. */
export interface AppleBillingIdentity {
  apiOrigin: string;
  revision: number;
  principalId: string | null;
}

export class AppleBillingSessionChangedError extends Error {
  readonly code = "apple_billing_session_changed";

  constructor() {
    super("apple_billing_session_changed");
    this.name = "AppleBillingSessionChangedError";
  }
}

export interface AppleBillingSessionOptions {
  userId: string;
  openSecretApiUrl: string;
  billingUrl: string;
  auth: Pick<OpenSecretContextType, "generateThirdPartyToken">;
  readIdentity: () => AppleBillingIdentity;
  bridge: StoreKitBridge;
  fetch?: AppleBillingRequestOptions["fetch"];
  allowInsecureLoopback?: boolean;
  retryPolicy?: AppleBillingRetryPolicy;
  /** Synchronous notification only; consumers fence their own later async work. */
  onAcknowledged?: (response: AppleTransactionResponse) => void;
  onListenerError?: (error: unknown) => void;
  /** Called only after a listener-delivered transaction has also finished. */
  onListenerRecovered?: () => void;
}

export type AppleBillingPurchaseResult =
  | { status: "pending" | "cancelled" }
  | { status: "success"; acknowledgement: AppleTransactionResponse };

/**
 * One immutable credential revision owns this instance, its token cache, native
 * listener and recovery queue. Dispose synchronously before logout/deletion or
 * account replacement. A revoked instance can never be reactivated, even for
 * the same user. SDK refresh also advances revision: create a fresh session and
 * recover after refresh rather than adopting the new credentials in place.
 *
 * No background retries, UI mounting, provider selection, or JWS persistence.
 */
export class AppleBillingSession {
  private readonly options: AppleBillingSessionOptions;
  private readonly identity: AppleBillingIdentity;
  private readonly abort = new AbortController();
  private readonly recovery: StoreKitRecovery<AppleTransactionResponse>;
  private readonly retryPolicy: AppleBillingRetryPolicy;
  private active = true;
  private token: string | undefined;
  private tokenRequest: Promise<string> | undefined;
  private accountTokenRequest: Promise<string> | undefined;
  private listenerRequest: Promise<void> | undefined;
  private removeListener: (() => void) | undefined;

  constructor(input: AppleBillingSessionOptions) {
    const options = {
      ...input,
      auth: { generateThirdPartyToken: input.auth.generateThirdPartyToken.bind(input.auth) }
    };
    this.options = options;
    this.retryPolicy = options.retryPolicy ?? new AppleBillingRetryPolicy();
    // Project even an injected reader's result: credential-bearing extra fields
    // must not become part of the retained session owner.
    const { apiOrigin, revision, principalId } = options.readIdentity();
    this.identity = { apiOrigin, revision, principalId };
    if (
      !options.userId ||
      principalId !== options.userId ||
      apiOrigin !== new URL(options.openSecretApiUrl).origin ||
      !Number.isSafeInteger(revision) ||
      revision < 0
    ) {
      throw new AppleBillingSessionChangedError();
    }
    this.recovery = new StoreKitRecovery(
      {
        getSignedTransactions: async () => {
          const result = await this.native(() => options.bridge.getSignedTransactions());
          for (const transaction of result.transactions) this.observe(transaction);
          return result;
        },
        sync: () => this.native(() => options.bridge.sync()),
        finishTransaction: (id) => this.native(() => options.bridge.finishTransaction(id))
      },
      async (signedTransaction, expectedTransactionId) => {
        this.assertCurrent();
        this.retryPolicy.assertMaySubmit(expectedTransactionId, signedTransaction);
        let response: AppleTransactionResponse;
        try {
          response = await this.request((request) =>
            submitAppleTransaction({ ...request, signedTransaction, expectedTransactionId })
          );
        } catch (error) {
          this.assertCurrent();
          this.retryPolicy.failed(expectedTransactionId, signedTransaction, error);
          throw error;
        }
        this.notify(options.onAcknowledged, response);
        this.assertCurrent();
        return response;
      }
    );
  }

  /** Also use immediately before publishing results returned by this session. */
  assertCurrent(): void {
    if (!this.active) throw new AppleBillingSessionChangedError();
    try {
      const current = this.options.readIdentity();
      if (
        current.apiOrigin === this.identity.apiOrigin &&
        current.revision === this.identity.revision &&
        current.principalId === this.identity.principalId
      ) {
        return;
      }
    } catch {
      // Failure to read credential authority must also revoke this owner.
    }
    this.dispose();
    throw new AppleBillingSessionChangedError();
  }

  dispose(): void {
    if (!this.active) return;
    this.active = false;
    this.abort.abort();
    this.recovery.dispose();
    this.token = undefined;
    this.tokenRequest = undefined;
    this.accountTokenRequest = undefined;
    if (this.removeListener) this.releaseListener(this.removeListener);
    this.removeListener = undefined;
  }

  private async native<T>(call: () => Promise<T>): Promise<T> {
    this.assertCurrent();
    // No await between the ownership check and invoking the native operation.
    // Once invoked, StoreKit purchase/finish cannot be revoked by JS disposal.
    try {
      const result = await call();
      this.assertCurrent();
      return result;
    } catch (error) {
      this.assertCurrent();
      throw error;
    }
  }

  private billingToken(): Promise<string> {
    this.assertCurrent();
    if (this.token !== undefined) return Promise.resolve(this.token);
    if (this.tokenRequest) return this.tokenRequest;
    const request = this.mintToken();
    this.tokenRequest = request;
    void request
      .finally(() => {
        if (this.tokenRequest === request) this.tokenRequest = undefined;
      })
      .catch(() => {});
    return request;
  }

  private async mintToken(): Promise<string> {
    this.assertCurrent();
    let result: { token: string };
    try {
      result = await this.options.auth.generateThirdPartyToken(this.options.billingUrl);
    } catch {
      this.assertCurrent();
      throw new Error("apple_billing_auth_unavailable");
    }
    this.assertCurrent();
    if (typeof result.token !== "string" || !result.token) {
      throw new Error("apple_billing_auth_unavailable");
    }
    this.token = result.token;
    return result.token;
  }

  private async request<T>(call: (options: AppleBillingRequestOptions) => Promise<T>): Promise<T> {
    let token = await this.billingToken();
    for (let attempt = 0; attempt < 2; attempt++) {
      this.assertCurrent();
      try {
        const result = await call({
          billingUrl: this.options.billingUrl,
          token,
          signal: this.abort.signal,
          fetch: this.options.fetch,
          allowInsecureLoopback: this.options.allowInsecureLoopback
        });
        this.assertCurrent();
        return result;
      } catch (error) {
        this.assertCurrent();
        if (!(error instanceof AppleBillingApiError) || error.status !== 401 || attempt !== 0) {
          throw error;
        }
        // A late 401 for an old token must not discard a newer single-flight mint.
        if (this.token === token) this.token = undefined;
        token = await this.billingToken();
      }
    }
    throw new Error("apple_billing_auth_unavailable");
  }

  getAccountToken(): Promise<string> {
    this.assertCurrent();
    if (this.accountTokenRequest) return this.accountTokenRequest;
    const request = this.request(fetchAppleAccountToken).then((response) => {
      this.assertCurrent();
      return response.app_account_token;
    });
    this.accountTokenRequest = request;
    void request.catch(() => {
      if (this.accountTokenRequest === request) this.accountTokenRequest = undefined;
    });
    return request;
  }

  async purchase(productId: string): Promise<AppleBillingPurchaseResult> {
    const token = await this.getAccountToken();
    const result = await this.native(() => this.options.bridge.purchase(productId, token));
    if (result.status !== "success") return result;
    return { status: "success", acknowledgement: await this.acknowledge(result.transaction) };
  }

  acknowledge(transaction: SignedStoreKitTransaction): Promise<AppleTransactionResponse> {
    this.assertCurrent();
    this.observe(transaction);
    return this.recovery.acknowledge(transaction);
  }

  private observe(transaction: SignedStoreKitTransaction): void {
    this.retryPolicy.observe(transaction.transactionId, transaction.jws);
  }

  recover(): Promise<AppleTransactionResponse[]> {
    this.assertCurrent();
    return this.recovery.recover();
  }

  restore(): Promise<AppleTransactionResponse[]> {
    this.assertCurrent();
    this.retryPolicy.retry();
    return this.recovery.restore();
  }

  /** Register before initial enumeration so launch-time delivery cannot race it. */
  async start(): Promise<AppleTransactionResponse[]> {
    this.assertCurrent();
    if (!this.listenerRequest) {
      const request = this.listen();
      this.listenerRequest = request;
      void request.catch(() => {
        if (this.listenerRequest === request) this.listenerRequest = undefined;
      });
    }
    await this.listenerRequest;
    this.assertCurrent();
    return this.recover();
  }

  private async listen(): Promise<void> {
    const remove = await this.options.bridge.onTransaction((transaction) => {
      try {
        // Register observed revisions synchronously, before an older pending
        // submission can continue into finish on its next microtask.
        void this.acknowledge(transaction).then(
          () => this.notify(this.options.onListenerRecovered, undefined),
          (error: unknown) => this.notify(this.options.onListenerError, error)
        );
      } catch (error) {
        this.notify(this.options.onListenerError, error);
      }
    });
    try {
      this.assertCurrent();
      this.removeListener = remove;
    } catch (error) {
      this.releaseListener(remove);
      throw error;
    }
  }

  private releaseListener(remove: () => void): void {
    try {
      // The Tauri listener implementation returns an asynchronous unregister,
      // despite the bridge's void cleanup type. Always consume its rejection.
      void Promise.resolve(remove()).catch(() => {});
    } catch {
      // The revoked owner still fences callbacks if native cleanup fails.
    }
  }

  private notify<T>(callback: ((value: T) => void) | undefined, value: T): void {
    try {
      this.assertCurrent();
      // Observer failures must not change durable acknowledgement/finish rules.
      void Promise.resolve(callback?.(value)).catch(() => {});
    } catch {
      // Never publish an obsolete owner's result or log callback payloads.
    }
  }
}

/** Uses the shipped SDK and native bridge; no production UI is mounted here. */
export function createAppleBillingSession(
  options: Omit<AppleBillingSessionOptions, "readIdentity" | "bridge">
): AppleBillingSession {
  if (!isIOS()) throw new Error("apple_billing_requires_ios");
  return new AppleBillingSession({
    ...options,
    bridge: storeKit,
    readIdentity: () => {
      const { apiOrigin, revision, principalId } = readNativeUserAuth(options.openSecretApiUrl);
      return { apiOrigin, revision, principalId };
    }
  });
}
