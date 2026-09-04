import { describe, expect, test } from "bun:test";
import type { NativeUserAuthState } from "@mapleai/sdk";
import {
  MapleApiAuthService,
  type MapleApiAuthBridge,
  type MapleApiAuthSnapshot
} from "./mapleApiAuthService";

const API_ORIGIN = "https://enclave.trymaple.ai";
const CACHE_ROOT = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

function deferred(): { promise: Promise<void>; resolve: () => void } {
  let resolve = () => {};
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function authState(principalId: string | null = "user-a", revision = 1): NativeUserAuthState {
  return {
    apiOrigin: API_ORIGIN,
    revision,
    principalId,
    credentials: principalId
      ? { accessToken: `${principalId}-access`, refreshToken: `${principalId}-refresh` }
      : null,
    cacheNamespaceRootBase64: CACHE_ROOT
  };
}

class FakeAuthBridge implements MapleApiAuthBridge {
  browserAuth = authState();
  desktop = true;
  installRootCalls = 0;
  setCalls = 0;
  clearCalls = 0;
  commandOrder: string[] = [];
  installRootHook: (() => Promise<void>) | null = null;
  setHook: (() => Promise<void>) | null = null;
  lastSetRequest: Record<string, unknown> | null = null;

  isDesktop(): boolean {
    return this.desktop;
  }

  apiUrl(): string {
    return API_ORIGIN;
  }

  readAuth(): NativeUserAuthState {
    return structuredClone(this.browserAuth);
  }

  async installRoot(apiUrl: string): Promise<void> {
    expect(apiUrl).toBe(API_ORIGIN);
    this.installRootCalls += 1;
    this.commandOrder.push("root:start");
    await this.installRootHook?.();
    this.commandOrder.push("root:finish");
  }

  async invoke<T>(command: string, args: Record<string, unknown>): Promise<T> {
    if (command === "maple_api_set_auth") {
      this.setCalls += 1;
      this.commandOrder.push("set:start");
      this.lastSetRequest = structuredClone(args.request as Record<string, unknown>);
      await this.setHook?.();
      this.commandOrder.push("set:finish");
      return {
        userId: (args.request as { userId: string }).userId,
        nativeInstanceId: "native-instance-1",
        revision: this.setCalls
      } as T;
    }
    if (command === "maple_api_clear_auth") {
      this.clearCalls += 1;
      this.commandOrder.push("clear");
      return undefined as T;
    }
    throw new Error(`Unexpected command: ${command}`);
  }
}

describe("MapleApiAuthService", () => {
  test("installs one fenced credential snapshot and never syncs it per operation", async () => {
    const bridge = new FakeAuthBridge();
    const service = new MapleApiAuthService(bridge);

    await service.activate("USER-A");
    await service.sync("user-a");
    await service.sync("user-a");

    expect(bridge.installRootCalls).toBe(1);
    expect(bridge.setCalls).toBe(1);
    expect(bridge.commandOrder).toEqual(["root:start", "root:finish", "set:start", "set:finish"]);
    expect(bridge.lastSetRequest).toEqual({
      userId: "user-a",
      apiUrl: API_ORIGIN,
      accessToken: "user-a-access",
      refreshToken: "user-a-refresh",
      cacheNamespaceRootBase64: CACHE_ROOT
    });
  });

  test("fails before native installation if browser authority changes while root is installed", async () => {
    const bridge = new FakeAuthBridge();
    bridge.installRootHook = async () => {
      bridge.browserAuth = authState("user-b", 2);
    };
    const service = new MapleApiAuthService(bridge);

    await expect(service.activate("user-a")).rejects.toThrow("current signed-in account");
    expect(bridge.setCalls).toBe(0);
    expect(bridge.clearCalls).toBe(0);
  });

  test("clears the installed native client if browser authority changes during validation", async () => {
    const bridge = new FakeAuthBridge();
    bridge.setHook = async () => {
      bridge.browserAuth = authState("user-b", 2);
    };
    const service = new MapleApiAuthService(bridge);

    await expect(service.activate("user-a")).rejects.toThrow("current signed-in account");
    expect(bridge.setCalls).toBe(1);
    expect(bridge.clearCalls).toBe(1);
    expect(bridge.commandOrder.at(-1)).toBe("clear");
  });

  test("rejects a malformed native receipt and clears the candidate", async () => {
    const bridge = new FakeAuthBridge();
    const originalInvoke = bridge.invoke.bind(bridge);
    bridge.invoke = async <T>(command: string, args: Record<string, unknown>): Promise<T> => {
      if (command !== "maple_api_set_auth") return await originalInvoke<T>(command, args);
      bridge.setCalls += 1;
      return {
        userId: "user-b",
        nativeInstanceId: "native-instance-1",
        revision: 1
      } as T;
    };
    const service = new MapleApiAuthService(bridge);

    await expect(service.activate("user-a")).rejects.toThrow("changed while credentials");
    expect(bridge.clearCalls).toBe(1);
  });

  test("serialized clear cannot be overtaken by a delayed installation", async () => {
    const bridge = new FakeAuthBridge();
    const setStarted = deferred();
    const releaseSet = deferred();
    bridge.setHook = async () => {
      setStarted.resolve();
      await releaseSet.promise;
    };
    const service = new MapleApiAuthService(bridge);

    const activation = service.activate("user-a");
    await setStarted.promise;
    const clearing = service.clear("user-a");
    releaseSet.resolve();
    await Promise.all([activation, clearing]);

    expect(bridge.commandOrder).toEqual([
      "root:start",
      "root:finish",
      "set:start",
      "set:finish",
      "clear"
    ]);
  });

  test("inactive and cross-account operation gates do not reinstall credentials", async () => {
    const bridge = new FakeAuthBridge();
    const service = new MapleApiAuthService(bridge);

    await expect(service.sync("user-a")).rejects.toThrow("changed before the operation");
    await service.activate("user-a");
    await expect(service.sync("user-b")).rejects.toThrow("changed before the operation");
    expect(bridge.setCalls).toBe(1);
  });

  test("web clients do not invoke the native bridge", async () => {
    const bridge = new FakeAuthBridge();
    bridge.desktop = false;
    const service = new MapleApiAuthService(bridge);

    await service.activate("user-a");
    await service.sync("user-a");
    await service.clear("user-a");

    expect(bridge.installRootCalls).toBe(0);
    expect(bridge.setCalls).toBe(0);
    expect(bridge.clearCalls).toBe(0);
  });

  test("native receipts remain non-secret", () => {
    const receipt: MapleApiAuthSnapshot = {
      userId: "user-a",
      nativeInstanceId: "native-instance-1",
      revision: 1
    };
    expect(Object.keys(receipt).sort()).toEqual(["nativeInstanceId", "revision", "userId"]);
  });
});
