import {
  AppleBillingSessionChangedError,
  type AppleBillingIdentity,
  type AppleBillingPurchaseResult,
  type AppleBillingSession
} from "./appleBillingSession";
import { AppleBillingApiError, type AppleTransactionResponse } from "./appleBillingApi";
import { StoreKitRecoveryError } from "@/services/storeKitService";
import { AppleBillingRetryPolicy, shouldRetryAppleBilling } from "./appleBillingRetryPolicy";

type Session = Pick<
  AppleBillingSession,
  "assertCurrent" | "dispose" | "start" | "purchase" | "restore"
>;

export interface AppleBillingLifecycleState {
  ownerKey: string | null;
  ready: boolean;
  busy: boolean;
  recoveryError: string | null;
}

interface Options {
  readIdentity(): AppleBillingIdentity;
  createSession(
    userId: string,
    onAcknowledged: (response: AppleTransactionResponse) => void,
    onListenerError: (error: unknown) => void,
    retryPolicy: AppleBillingRetryPolicy,
    onListenerRecovered: () => void
  ): Session;
  onAcknowledged(response: AppleTransactionResponse, userId: string): void;
  now?: () => number;
}

const RETRY_DELAYS = [15_000, 30_000, 60_000, 120_000, 300_000];

/** Bounded public messages only: never surface a JWS, native error, or response body. */
export function appleBillingErrorMessage(error: unknown): string {
  if (error instanceof AppleBillingSessionChangedError) {
    return "Your account changed. Please try again.";
  }
  if (error instanceof AppleBillingApiError && error.code === "conflict") {
    return "This Apple subscription belongs to another Maple account. Sign in to that account or contact support.";
  }
  if (error instanceof StoreKitRecoveryError && error.failures.length > 0) {
    const conflict = error.failures.find(({ error: failure }) => hasOwnershipConflict(failure));
    if (conflict) return appleBillingErrorMessage(conflict.error);
  }
  return "We couldn't confirm your Apple purchases. Your purchases are saved by Apple; try again when you're connected.";
}

function hasOwnershipConflict(error: unknown): boolean {
  return (
    (error instanceof AppleBillingApiError && error.code === "conflict") ||
    (error instanceof StoreKitRecoveryError &&
      error.failures.some((failure) => hasOwnershipConflict(failure.error)))
  );
}

/**
 * Runtime owner for one mounted iOS app. Sessions themselves fence every await
 * against SDK authority; the periodic check only reattaches recovery after a
 * refresh. No credentials or signed transactions are persisted by this layer.
 */
export class AppleBillingLifecycle {
  private userId: string | null = null;
  private identity: AppleBillingIdentity | null = null;
  private session: Session | null = null;
  private stopped = true;
  private readonly suspensions = new Map<string, number>();
  private operations = 0;
  private recovering: Promise<AppleTransactionResponse[]> | null = null;
  private retryAt: number | null = null;
  private failures = 0;
  private failureVersion = 0;
  private ownerSequence = 0;
  private retryPolicy = new AppleBillingRetryPolicy();
  private retryOwner: { userId: string; apiOrigin: string } | null = null;
  private readonly listeners = new Set<() => void>();
  private state: AppleBillingLifecycleState = {
    ownerKey: null,
    ready: false,
    busy: false,
    recoveryError: null
  };

  constructor(private readonly options: Options) {}

  /** Remount starts a fresh session; a disposed credential owner is never reused. */
  activate(): void {
    this.stopped = false;
    this.tick();
  }

  getSnapshot = (): AppleBillingLifecycleState => this.state;
  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  private publish(patch: Partial<AppleBillingLifecycleState>): void {
    const next = { ...this.state, ...patch };
    if (
      next.ownerKey === this.state.ownerKey &&
      next.ready === this.state.ready &&
      next.busy === this.state.busy &&
      next.recoveryError === this.state.recoveryError
    )
      return;
    this.state = next;
    for (const listener of this.listeners) listener();
  }

  setUser(userId: string | null): void {
    if (this.userId !== userId) {
      this.revoke();
      this.resetRetryPolicy();
      this.userId = userId;
    }
    this.tick();
  }

  /** Synchronous disposal before the first await in logout/account deletion. */
  suspend(userId: string): () => void {
    if (this.userId !== userId || this.stopped) return () => {};
    this.suspensions.set(userId, (this.suspensions.get(userId) ?? 0) + 1);
    this.revoke();
    let released = false;
    return () => {
      if (released) return;
      released = true;
      const remaining = (this.suspensions.get(userId) ?? 1) - 1;
      if (remaining === 0) this.suspensions.delete(userId);
      else this.suspensions.set(userId, remaining);
      this.tick();
    };
  }

  dispose(): void {
    this.stopped = true;
    this.revoke();
    this.resetRetryPolicy();
  }

  private resetRetryPolicy(): void {
    this.retryPolicy = new AppleBillingRetryPolicy();
    this.retryOwner = null;
  }

  private revoke(): void {
    this.session?.dispose();
    this.session = null;
    this.identity = null;
    this.operations = 0;
    this.recovering = null;
    this.retryAt = null;
    this.failures = 0;
    this.publish({ ownerKey: null, ready: false, busy: false, recoveryError: null });
  }

  tick = (): void => {
    if (this.stopped || !this.userId || this.suspensions.has(this.userId)) return;
    let identity: AppleBillingIdentity;
    try {
      identity = this.options.readIdentity();
      if (identity.principalId !== this.userId) throw new AppleBillingSessionChangedError();
    } catch {
      this.revoke();
      return;
    }
    if (
      !this.identity ||
      identity.apiOrigin !== this.identity.apiOrigin ||
      identity.principalId !== this.identity.principalId ||
      identity.revision !== this.identity.revision
    ) {
      this.revoke();
      const owner = this.userId;
      if (this.retryOwner?.userId !== owner || this.retryOwner.apiOrigin !== identity.apiOrigin) {
        this.resetRetryPolicy();
        this.retryOwner = { userId: owner, apiOrigin: identity.apiOrigin };
      }
      try {
        const session = this.options.createSession(
          owner,
          (response) => {
            if (!this.current(session)) return;
            this.options.onAcknowledged(response, owner);
          },
          (error) => {
            if (this.current(session)) this.failed(error);
          },
          this.retryPolicy,
          () => this.listenerRecovered(session)
        );
        this.session = session;
        this.identity = identity;
        this.publish({ ownerKey: `apple-session:${++this.ownerSequence}`, ready: true });
        void this.recover().catch(() => {});
      } catch {
        this.revoke();
      }
    } else if (this.retryAt !== null && this.now() >= this.retryAt && !this.recovering) {
      void this.recover().catch(() => {});
    }
  };

  private now(): number {
    return (this.options.now ?? Date.now)();
  }

  private listenerRecovered(session: Session): void {
    // Do not clear another ID's failure just because this listener finished.
    // Wait for any overlapping enumeration, then reconcile all pending IDs.
    void Promise.resolve(this.recovering)
      .catch(() => {})
      .then(() => {
        if (this.current(session) && this.state.recoveryError) {
          return this.recoverAutomatically();
        }
      })
      .catch(() => {});
  }

  private current(session: Session): boolean {
    if (
      this.stopped ||
      !this.userId ||
      this.suspensions.has(this.userId) ||
      this.session !== session
    )
      return false;
    try {
      session.assertCurrent();
      return true;
    } catch {
      return false;
    }
  }

  private requiredSession(): Session {
    this.tick();
    const session = this.session;
    if (!session || !this.current(session)) throw new AppleBillingSessionChangedError();
    return session;
  }

  assertOwner(ownerKey: string | null): void {
    this.requiredSession();
    if (!ownerKey || this.state.ownerKey !== ownerKey) throw new AppleBillingSessionChangedError();
  }

  private failed(error: unknown): void {
    this.failureVersion++;
    if (shouldRetryAppleBilling(error)) {
      this.retryAt = this.now() + RETRY_DELAYS[Math.min(this.failures++, RETRY_DELAYS.length - 1)];
    }
    this.publish({ recoveryError: appleBillingErrorMessage(error) });
  }

  private async run<T>(
    session: Session,
    operation: () => Promise<T>,
    clearsRecovery = false
  ): Promise<T> {
    const failureVersion = this.failureVersion;
    this.operations++;
    this.publish({ busy: true });
    try {
      const result = await operation();
      if (!this.current(session)) throw new AppleBillingSessionChangedError();
      if (clearsRecovery && this.failureVersion === failureVersion) {
        this.failures = 0;
        this.retryAt = null;
        this.publish({ recoveryError: null });
      }
      return result;
    } catch (error) {
      if (this.current(session)) this.failed(error);
      throw error;
    } finally {
      if (this.session === session) {
        this.operations--;
        this.publish({ busy: this.operations > 0 });
      }
    }
  }

  private recover(): Promise<AppleTransactionResponse[]> {
    if (this.recovering) return this.recovering;
    const session = this.session;
    if (!session || !this.current(session))
      return Promise.reject(new AppleBillingSessionChangedError());
    this.retryAt = null;
    const request = this.run(session, () => session.start(), true);
    this.recovering = request;
    void request
      .finally(() => {
        if (this.recovering === request) this.recovering = null;
      })
      .catch(() => {});
    return request;
  }

  retry = async (): Promise<void> => {
    this.requiredSession();
    this.retryPolicy.retry();
    await this.recover();
  };

  /** Foreground/network events enumerate new evidence without lifting 400/409 blocks. */
  recoverAutomatically = async (): Promise<void> => {
    this.requiredSession();
    if (this.retryAt !== null && this.now() < this.retryAt) return;
    await this.recover();
  };

  purchase = async (productId: string): Promise<AppleBillingPurchaseResult> => {
    const session = this.requiredSession();
    if (this.state.busy) throw new Error("apple_billing_busy");
    return this.run(session, () => session.purchase(productId));
  };

  restore = async (): Promise<AppleTransactionResponse[]> => {
    const session = this.requiredSession();
    if (this.state.busy) throw new Error("apple_billing_busy");
    return this.run(session, () => session.restore(), true);
  };
}

const mountedLifecycles = new Set<AppleBillingLifecycle>();

export function registerAppleBillingLifecycle(lifecycle: AppleBillingLifecycle): () => void {
  mountedLifecycles.add(lifecycle);
  return () => mountedLifecycles.delete(lifecycle);
}

export function suspendAppleBillingForAccount(userId: string | null | undefined): () => void {
  if (!userId) return () => {};
  const releases = [...mountedLifecycles].map((lifecycle) => lifecycle.suspend(userId));
  return () => releases.forEach((release) => release());
}
