import crypto from "node:crypto";
import { syncBuiltinESMExports } from "node:module";
import { vi } from "vitest";

/** Install before importing Pi. Microtasks and setImmediate remain real. */
export function deterministicEnvironment(epochMs: number) {
  if (!Number.isSafeInteger(epochMs) || epochMs < 0) throw new Error("Clock epoch must be a nonnegative safe integer");
  let seed = 0x12345678;
  let uuid = 0;
  const next = () => {
    seed ^= seed << 13;
    seed ^= seed >>> 17;
    seed ^= seed << 5;
    return seed >>> 0;
  };
  vi.useFakeTimers({
    toFake: ["Date", "setTimeout", "clearTimeout", "setInterval", "clearInterval"],
    now: epochMs,
  });
  vi.spyOn(Math, "random").mockImplementation(() => next() / 0x100000000);
  vi.spyOn(crypto, "randomUUID").mockImplementation(() =>
    `${(++uuid).toString(16).padStart(8, "0")}-0000-4000-8000-000000000000`);
  vi.spyOn(globalThis.crypto, "getRandomValues").mockImplementation((buffer) => {
    if (buffer === null) throw new TypeError("Expected an integer typed array");
    const bytes = new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength);
    for (let i = 0; i < bytes.length; i++) bytes[i] = next() & 255;
    return buffer;
  });
  syncBuiltinESMExports();
  const restore = () => {
    vi.useRealTimers();
    vi.restoreAllMocks();
    syncBuiltinESMExports();
  };
  return {
    restore,
    advance: (milliseconds: number) => vi.advanceTimersByTimeAsync(milliseconds),
    setNow: (milliseconds: number) => vi.setSystemTime(milliseconds),
    pendingTimers: () => vi.getTimerCount(),
  };
}
