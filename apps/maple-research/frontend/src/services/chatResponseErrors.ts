const ERROR_CONTRACT_HEADER = "x-opensecret-error-contract";
const ERROR_CODE_HEADER = "x-opensecret-error-code";
const ERROR_CONTRACT_VERSION = "1";
const IMAGE_DESCRIPTION_UNAVAILABLE_ERROR_CODE = "image_description_unavailable";
const IMAGE_DESCRIPTION_UNAVAILABLE_STATUS = 503;
const MAX_ERROR_CAUSE_DEPTH = 4;
const MAX_LEGACY_ERROR_LENGTH = 4_096;
const REQUEST_NOT_DISPATCHED_CODE = "opensecret_request_not_dispatched";

type ErrorResponseMetadata = {
  status?: unknown;
  headers?: unknown;
  cause?: unknown;
  code?: unknown;
  error?: unknown;
  message?: unknown;
  requestDispatchCode?: unknown;
  definitelyNotDispatched?: unknown;
};

export type ChatLimitFailure =
  | { kind: "usage"; status: 403; code: "usage_limit_reached" }
  | { kind: "freeToken"; status: 403; code: "free_tier_token_limit_exceeded" }
  | { kind: "context"; status: 413; code: "message_exceeds_context_limit" };

function limitFailureForCode(status: unknown, code: unknown): ChatLimitFailure | null {
  if (status === 403 && code === "usage_limit_reached") {
    return { kind: "usage", status, code };
  }
  if (status === 403 && code === "free_tier_token_limit_exceeded") {
    return { kind: "freeToken", status, code };
  }
  if (status === 413 && code === "message_exceeds_context_limit") {
    return { kind: "context", status, code };
  }
  return null;
}

type LegacyStatusFailure = {
  status: 403 | 413;
  message: string;
  code?: unknown;
  error?: unknown;
};

function legacyStatusFailure(metadata: ErrorResponseMetadata): LegacyStatusFailure | null {
  if (typeof metadata.message !== "string" || metadata.message.length > MAX_LEGACY_ERROR_LENGTH) {
    return null;
  }
  for (const status of [403, 413] as const) {
    const prefix = `Request failed with status ${status}:`;
    if (!metadata.message.startsWith(prefix)) continue;
    if (metadata.status !== undefined && metadata.status !== status) return null;
    try {
      const body: unknown = JSON.parse(metadata.message.slice(prefix.length));
      if (typeof body !== "object" || body === null) return null;
      const legacy = body as {
        status?: unknown;
        message?: unknown;
        code?: unknown;
        error?: unknown;
      };
      if (legacy.status !== status || typeof legacy.message !== "string") return null;
      return { ...legacy, status, message: legacy.message };
    } catch {
      return null;
    }
  }
  return null;
}

function legacyLimitFailure(legacy: LegacyStatusFailure): ChatLimitFailure | null {
  const code =
    legacy.message === "Usage limit reached"
      ? "usage_limit_reached"
      : legacy.message === "Free tier token limit exceeded"
        ? "free_tier_token_limit_exceeded"
        : legacy.message === "Message exceeds context limit"
          ? "message_exceeds_context_limit"
          : null;
  const nestedCode =
    typeof legacy.error === "object" && legacy.error !== null
      ? (legacy.error as { code?: unknown }).code
      : undefined;
  if (
    [legacy.code, nestedCode].some(
      (legacyCode) => legacyCode !== undefined && legacyCode !== null && legacyCode !== code
    )
  ) {
    return null;
  }
  return limitFailureForCode(legacy.status, code);
}

function errorCauseChain(error: unknown): readonly ErrorResponseMetadata[] {
  const chain: ErrorResponseMetadata[] = [];
  const seen = new Set<object>();
  let current = error;

  for (let depth = 0; depth < MAX_ERROR_CAUSE_DEPTH; depth += 1) {
    if (typeof current !== "object" || current === null || seen.has(current)) break;
    seen.add(current);
    const metadata = current as ErrorResponseMetadata;
    chain.push(metadata);
    current = metadata.cause;
  }

  return chain;
}

type ResolvedChatResponseError =
  | { status: unknown; code: unknown; legacy?: never }
  | { status: 403 | 413; legacy: LegacyStatusFailure; code?: never };

function resolveChatResponseError(error: unknown): ResolvedChatResponseError | null {
  for (const metadata of errorCauseChain(error)) {
    const headers = metadata.headers instanceof Headers ? metadata.headers : null;
    const contract = headers?.get(ERROR_CONTRACT_HEADER);
    const headerCode = headers?.get(ERROR_CODE_HEADER);
    const body =
      typeof metadata.error === "object" && metadata.error !== null
        ? (metadata.error as { code?: unknown })
        : null;
    const codes = [headerCode, metadata.code, body?.code].filter(
      (code) => code !== undefined && code !== null
    );

    // The OpenAI client retains both APIError.code and APIError.error.code.
    // Require every supplied source to agree; an unknown or conflicting code
    // must not fall back to an old human-readable message.
    if (contract !== undefined && contract !== null && contract !== ERROR_CONTRACT_VERSION) {
      return null;
    }
    if (codes.length > 0) {
      if (headerCode !== undefined && headerCode !== null && contract !== ERROR_CONTRACT_VERSION) {
        return null;
      }
      if (codes.some((code) => typeof code !== "string" || code !== codes[0])) return null;
      return { status: metadata.status, code: codes[0] };
    }

    const legacy = legacyStatusFailure(metadata);
    if (legacy) return { status: legacy.status, legacy };
    // Do not combine a wrapper's status with a cause's code. A concrete HTTP
    // rejection without a known limit remains an ordinary application error.
    if (metadata.status !== undefined) return null;
  }
  return null;
}

/** Classify a specific server limit, never a generic HTTP denial or display message. */
export function classifyChatLimitFailure(error: unknown): ChatLimitFailure | null {
  const resolved = resolveChatResponseError(error);
  if (!resolved) return null;
  return resolved.legacy
    ? legacyLimitFailure(resolved.legacy)
    : limitFailureForCode(resolved.status, resolved.code);
}

/** This code also denies unpaid features, so it must not imply a model upsell. */
export function isChatPlanAccessDeniedError(error: unknown): boolean {
  const resolved = resolveChatResponseError(error);
  return resolved?.status === 403 && resolved.code === "model_not_available_on_plan";
}

export function isChatRequestDefinitelyNotDispatchedError(error: unknown): boolean {
  return errorCauseChain(error).some(
    (metadata) =>
      metadata.requestDispatchCode === REQUEST_NOT_DISPATCHED_CODE &&
      metadata.definitelyNotDispatched === true
  );
}

/**
 * The Responses cancel endpoint returns 400 for an already-terminal race only
 * after its execution owner is quiescent. Other cancellation failures do not
 * certify that background work has stopped, even if a separate retrieve sees a
 * terminal database status.
 */
export function isChatResponseCancellationAlreadyTerminalError(error: unknown): boolean {
  return errorCauseChain(error).some((metadata) => metadata.status === 400);
}

/**
 * A non-timeout 4xx, or the explicit image-description pre-acceptance error,
 * rejected the turn before Responses persistence. The generic error-contract
 * version only describes the response schema, so other server failures remain
 * ambiguous.
 */
export function isChatResponseDefinitelyRejectedError(error: unknown): boolean {
  return errorCauseChain(error).some((metadata) => {
    const status = metadata.status ?? legacyStatusFailure(metadata)?.status;
    if (typeof status !== "number" || status === 408) return false;
    if (status >= 400 && status < 500) return true;
    return (
      metadata.status === IMAGE_DESCRIPTION_UNAVAILABLE_STATUS &&
      metadata.headers instanceof Headers &&
      metadata.headers.get(ERROR_CONTRACT_HEADER) === ERROR_CONTRACT_VERSION &&
      metadata.headers.get(ERROR_CODE_HEADER) === IMAGE_DESCRIPTION_UNAVAILABLE_ERROR_CODE
    );
  });
}

export function isImageDescriptionUnavailableError(error: unknown): boolean {
  for (const metadata of errorCauseChain(error)) {
    if (
      metadata.status === IMAGE_DESCRIPTION_UNAVAILABLE_STATUS &&
      metadata.headers instanceof Headers &&
      metadata.headers.get(ERROR_CONTRACT_HEADER) === ERROR_CONTRACT_VERSION &&
      metadata.headers.get(ERROR_CODE_HEADER) === IMAGE_DESCRIPTION_UNAVAILABLE_ERROR_CODE
    ) {
      return true;
    }
  }

  return false;
}
