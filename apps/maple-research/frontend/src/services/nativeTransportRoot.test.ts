import { describe, expect, test } from "bun:test";

import { ensureNativeTransportRoot } from "./nativeTransportRoot";

describe("ensureNativeTransportRoot", () => {
  test("installs one canonical root per origin and coalesces concurrent callers", async () => {
    const calls: Array<{ command: string; args?: Record<string, unknown> }> = [];
    const invokeNative = async <T>(command: string, args?: Record<string, unknown>): Promise<T> => {
      calls.push({ command, args });
      return undefined as T;
    };
    const apiUrl = `https://root-${crypto.randomUUID()}.example.test/v1`;

    await Promise.all([
      ensureNativeTransportRoot(apiUrl, invokeNative),
      ensureNativeTransportRoot(`${apiUrl}/`, invokeNative)
    ]);

    expect(calls).toHaveLength(1);
    expect(calls[0]?.command).toBe("install_native_transport_root");
    expect(calls[0]?.args?.apiUrl).toMatch(/^https:\/\/root-[a-f0-9-]+\.example\.test$/);
    expect(calls[0]?.args?.rootBase64).toMatch(/^[A-Za-z0-9+/]{43}=$/);
  });

  test("allows a later caller to retry a failed installation", async () => {
    let attempts = 0;
    const invokeNative = async <T>(): Promise<T> => {
      attempts += 1;
      if (attempts === 1) throw new Error("native unavailable");
      return undefined as T;
    };
    const apiUrl = `https://retry-${crypto.randomUUID()}.example.test`;

    await expect(ensureNativeTransportRoot(apiUrl, invokeNative)).rejects.toThrow(
      "native unavailable"
    );
    await expect(ensureNativeTransportRoot(apiUrl, invokeNative)).resolves.toBeUndefined();
    expect(attempts).toBe(2);
  });

  test("rechecks native state when browser persistence produces a different root", async () => {
    let installedRoot: unknown;
    let attempts = 0;
    const invokeNative = async <T>(
      _command: string,
      args?: Record<string, unknown>
    ): Promise<T> => {
      attempts += 1;
      if (installedRoot !== undefined && installedRoot !== args?.rootBase64) {
        throw new Error("native root mismatch");
      }
      installedRoot = args?.rootBase64;
      return undefined as T;
    };
    const apiUrl = `https://mismatch-${crypto.randomUUID()}.example.test`;

    await ensureNativeTransportRoot(apiUrl, invokeNative);
    localStorage.clear();

    await expect(ensureNativeTransportRoot(apiUrl, invokeNative)).rejects.toThrow(
      "native root mismatch"
    );
    expect(attempts).toBe(2);
  });
});
