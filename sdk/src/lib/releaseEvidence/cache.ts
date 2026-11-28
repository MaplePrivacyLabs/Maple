import type { ReleaseEvidenceResult } from "./types";

/*
 * Verified approvals held in memory, keyed by environment and PCR tuple. Only
 * passes are stored; a 404, a timeout or a failed verification is never
 * cached, so a later session always tries again.
 */

// A client meets one release per environment between updates, plus the
// occasional rollback; 32 leaves ample room while keeping the map small in a
// long-lived process.
const MAX_ENTRIES = 32;

const cache = new Map<string, ReleaseEvidenceResult>();

export function cacheKey(environment: string, pcr0: string, pcr1: string, pcr2: string): string {
  return `${environment}:${pcr0}:${pcr1}:${pcr2}`;
}

export function getCachedPass(key: string): ReleaseEvidenceResult | undefined {
  return cache.get(key);
}

export function storePass(key: string, result: ReleaseEvidenceResult): void {
  const isPass = result.outcome === "pass";
  if (!isPass) {
    return;
  }
  cache.set(key, result);
  // A Map iterates in insertion order, so the first key is the oldest. Each
  // call adds at most one entry, so at most one has to go.
  const overCapacity = cache.size > MAX_ENTRIES;
  if (overCapacity) {
    const oldest = cache.keys().next().value as string;
    cache.delete(oldest);
  }
}

/** Test hook: drop all cached passes. */
export function clearVerificationCache(): void {
  cache.clear();
}
