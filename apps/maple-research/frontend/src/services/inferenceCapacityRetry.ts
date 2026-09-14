import {
  findOpenSecretInferenceCapacityError,
  type OpenSecretInferenceCapacityError
} from "@mapleai/sdk";

async function waitForRetry(delayMs: number, signal: AbortSignal): Promise<void> {
  signal.throwIfAborted();
  if (delayMs === 0) return;

  await new Promise<void>((resolve, reject) => {
    const onAbort = () => {
      clearTimeout(timeout);
      reject(signal.reason ?? new DOMException("The operation was aborted.", "AbortError"));
    };
    const timeout = setTimeout(() => {
      signal.removeEventListener("abort", onAbort);
      resolve();
    }, delayMs);
    signal.addEventListener("abort", onAbort, { once: true });
  });
}

type InferenceSendLimit = 1 | 2;

/** Executes at most two inference sends across SDK repair and capacity replay. */
export async function withInferenceCapacityRetry<T>(
  send: (maxInferenceSends: InferenceSendLimit) => Promise<T>,
  signal: AbortSignal,
  onCapacityRejected?: (error: OpenSecretInferenceCapacityError) => void
): Promise<T> {
  signal.throwIfAborted();

  try {
    return await send(2);
  } catch (error) {
    const capacity = findOpenSecretInferenceCapacityError(error);
    if (!capacity) throw error;
    onCapacityRejected?.(capacity);
    if (capacity.retryDelayMs === null || capacity.inferenceSendCount !== 1) {
      throw error;
    }

    await waitForRetry(capacity.retryDelayMs, signal);
    signal.throwIfAborted();
    return send(1);
  }
}
