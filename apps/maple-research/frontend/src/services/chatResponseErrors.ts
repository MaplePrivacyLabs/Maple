const ERROR_CONTRACT_HEADER = "x-opensecret-error-contract";
const ERROR_CODE_HEADER = "x-opensecret-error-code";
const ERROR_CONTRACT_VERSION = "1";
const IMAGE_DESCRIPTION_UNAVAILABLE_ERROR_CODE = "image_description_unavailable";
const IMAGE_DESCRIPTION_UNAVAILABLE_STATUS = 503;
const MAX_ERROR_CAUSE_DEPTH = 4;
const REQUEST_NOT_DISPATCHED_CODE = "opensecret_request_not_dispatched";

type ErrorResponseMetadata = {
  status?: unknown;
  headers?: unknown;
  cause?: unknown;
  requestDispatchCode?: unknown;
  definitelyNotDispatched?: unknown;
};

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
    if (typeof metadata.status !== "number" || metadata.status === 408) return false;
    if (metadata.status >= 400 && metadata.status < 500) return true;
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
