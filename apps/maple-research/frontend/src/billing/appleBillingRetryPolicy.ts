import { AppleBillingApiError } from "./appleBillingApi";
import { StoreKitRecoveryError } from "@/services/storeKitService";

function requiresChangedInput(error: unknown): error is AppleBillingApiError {
  return (
    error instanceof AppleBillingApiError &&
    (error.status === 400 || error.status === 409 || error.code === "invalid_request")
  );
}

export function shouldRetryAppleBilling(error: unknown): boolean {
  if (error instanceof StoreKitRecoveryError && error.failures.length > 0) {
    return error.failures.some((failure) => shouldRetryAppleBilling(failure.error));
  }
  // In particular, 401 still gets the session's token refresh and 429/5xx
  // remain eligible for backoff. Do not classify every 4xx as permanent.
  return !requiresChangedInput(error);
}

/** Memory only, scoped to one Maple account and API origin, across SDK refreshes. */
export class AppleBillingRetryPolicy {
  private readonly transactions = new Map<
    string,
    { observed: Set<string>; rejected: Map<string, AppleBillingApiError> }
  >();

  observe(transactionId: string, jws: string): void {
    let transaction = this.transactions.get(transactionId);
    if (!transaction) {
      transaction = { observed: new Set(), rejected: new Map() };
      this.transactions.set(transactionId, transaction);
    }
    if (!transaction.observed.has(jws)) {
      transaction.observed.add(jws);
      // New signed state can resolve an earlier ownership/validation failure.
      // Repeated enumeration of an already-seen revision cannot lift the block.
      transaction.rejected.clear();
    }
  }

  assertMaySubmit(transactionId: string, jws: string): void {
    const error = this.transactions.get(transactionId)?.rejected.get(jws);
    if (error) throw error;
  }

  failed(transactionId: string, jws: string, error: unknown): void {
    if (requiresChangedInput(error)) {
      this.transactions.get(transactionId)?.rejected.set(jws, error);
    }
  }

  /** Explicit Retry/Restore can recover after support changes ownership/config. */
  retry(): void {
    for (const transaction of this.transactions.values()) transaction.rejected.clear();
  }
}
