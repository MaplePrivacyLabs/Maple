/*
 * Bounded fetch of a release bundle from the environment's static host.
 * The host is fixed by the locally configured environment; nothing in a
 * bundle, redirect or manifest can change it.
 */
import { MAX_BUNDLE_BYTES } from "./verification";

export const BUNDLE_HOSTS = {
  development: "https://attestations-dev.maple.ai",
  production: "https://attestations.maple.ai"
} as const;

const FETCH_TIMEOUT_MS = 5000;

/** The part of fetch the lookup uses. */
export type FetchBundle = (url: string, init: RequestInit) => Promise<Response>;

export type BundleFetch =
  | { ok: true; text: string }
  | {
      ok: false;
      category: "bundle-not-found" | "bundle-fetch-failed" | "bundle-timeout" | "bundle-too-large";
    };

/*
 * A leading byte order mark is kept, so JSON parsing rejects it as the Rust
 * SDK does. The default decoder, and response.text(), drop it silently.
 */
function decodeBundleText(bytes: Uint8Array): string {
  return new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes);
}

async function readBoundedText(response: Response, maxBytes: number): Promise<string | undefined> {
  if (!response.body) {
    const bytes = new Uint8Array(await response.arrayBuffer());
    const responseTooLarge = bytes.byteLength > maxBytes;
    if (responseTooLarge) {
      return undefined;
    }
    return decodeBundleText(bytes);
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let length = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      const chunkMissing = value === undefined || value === null;
      if (chunkMissing) {
        continue;
      }
      length += value.byteLength;
      const responseTooLarge = length > maxBytes;
      if (responseTooLarge) {
        await reader.cancel("bundle response too large").catch(() => {});
        return undefined;
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return decodeBundleText(bytes);
}

export async function fetchReleaseBundle(
  host: string,
  pcr0: string,
  fetchImpl: FetchBundle,
  timeoutMs: number = FETCH_TIMEOUT_MS
): Promise<BundleFetch> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetchImpl(`${host}/${pcr0}.json`, {
      method: "GET",
      headers: { accept: "application/json" },
      credentials: "omit",
      redirect: "error",
      signal: controller.signal
    });
    const notFound = response.status === 404;
    if (notFound) {
      await response.body?.cancel("bundle not found").catch(() => {});
      return { ok: false, category: "bundle-not-found" };
    }
    const unusableStatus = response.status !== 200;
    if (unusableStatus) {
      await response.body?.cancel("unusable bundle response").catch(() => {});
      return { ok: false, category: "bundle-fetch-failed" };
    }
    const text = await readBoundedText(response, MAX_BUNDLE_BYTES);
    const exceededCap = text === undefined;
    if (exceededCap) {
      return { ok: false, category: "bundle-too-large" };
    }
    return { ok: true, text };
  } catch {
    // Distinguish the deadline from other network failures; both fail closed.
    const deadlinePassed = controller.signal.aborted;
    if (deadlinePassed) {
      return { ok: false, category: "bundle-timeout" };
    }
    return { ok: false, category: "bundle-fetch-failed" };
  } finally {
    clearTimeout(timeout);
  }
}
