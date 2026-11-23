import type { PcrEnvironment } from "../pcr";

/**
 * Failure categories for the observation steps the shared verifier corpus
 * does not name; corpus reason codes are used unchanged for everything else.
 * The Rust SDK uses the same names.
 */
export type ReleaseEvidenceFailureCategory =
  // The configured environment has no approval key.
  | "approval-key-not-configured"
  // Bundle lookup failed before a verified result was available.
  | "bundle-fetch-failed"
  // Bundle lookup exceeded its network deadline.
  | "bundle-timeout"
  // The host answered 404 for this PCR0.
  | "bundle-not-found"
  // The response exceeded the byte cap before parsing.
  | "bundle-too-large"
  // The DSSE payload is not a well-formed in-toto statement.
  | "statement-malformed"
  // The statement type, payload type or predicate type is not the expected one.
  | "statement-unsupported"
  // The signed environment differs from the locally configured environment.
  | "statement-environment-mismatch"
  // The authenticated PCR tuple is missing or differs from the signed one.
  | "statement-measurement-mismatch"
  // Verification failed without a more specific shared or corpus reason code.
  | "verifier-failed";

/** Where the verified bundle text came from. */
export type ReleaseEvidenceSource = "embedded" | "cache" | "fetched";

/** Public identity of a verified release approval, safe to log. */
export interface ReleaseEvidenceIdentity {
  environment: PcrEnvironment;
  /** Lowercase hex PCR0 of the approved measurement. Public lookup key. */
  pcr0: string;
  /** Signed audit claims. Logged, never used as policy inputs. */
  repository: string;
  commit: string;
  /** The workflow run that built the release, and which attempt of it. */
  buildRunId: string;
  buildRunAttempt: number;
  /** Rekor v2 log index from the inclusion proof the verifier checked. */
  logIndex: string;
}

export type ReleaseEvidenceResult =
  | {
      outcome: "pass";
      environment: PcrEnvironment;
      pcr0: string;
      source: ReleaseEvidenceSource;
      identity: ReleaseEvidenceIdentity;
    }
  | {
      outcome: "fail";
      environment: PcrEnvironment;
      pcr0: string;
      category: ReleaseEvidenceFailureCategory | string;
      source?: ReleaseEvidenceSource;
    };
