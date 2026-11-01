import { afterEach, describe, expect, test } from "bun:test";
import { getApiClientApp, setApiUrl } from "../api";
import { snapshotClientApp, withClientAppHeader, type ClientApp } from "../clientApp";

afterEach(() => {
  setApiUrl("http://127.0.0.1:3000", { environment: "development" });
});

describe("client application reporting configuration", () => {
  test("accepts each known application and leaves an unconfigured SDK untagged", () => {
    for (const app of ["chat", "proxy", "work", undefined] as const) {
      expect(snapshotClientApp(app)).toBe(app);
    }
    expect(withClientAppHeader(undefined, undefined)).toBeUndefined();
  });

  test("rejects unsupported JavaScript configuration before changing the configured app", () => {
    setApiUrl("http://127.0.0.1:3000", undefined, "chat");
    for (const value of ["", "unknown", "Chat", "chat,work", null]) {
      expect(() => setApiUrl("http://127.0.0.1:3000", undefined, value as ClientApp)).toThrow(
        "clientApp must be"
      );
      expect(getApiClientApp()).toBe("chat");
    }
  });

  test("reconfiguration without an app clears the previous configured identity", () => {
    setApiUrl("http://127.0.0.1:3000", undefined, "work");
    expect(getApiClientApp()).toBe("work");
    setApiUrl("http://127.0.0.1:3000");
    expect(getApiClientApp()).toBeUndefined();
  });

  test("configured host replaces every caller attribution header without mutating input", () => {
    const input = [
      { name: "content-type", value: "application/json" },
      { name: "X-Maple-Client-App", value: "proxy" },
      { name: "x-maple-client-app", value: "work" },
      { name: "x-example", value: "preserved" }
    ];
    expect(withClientAppHeader(input, "chat")).toEqual([
      { name: "content-type", value: "application/json" },
      { name: "x-example", value: "preserved" },
      { name: "x-maple-client-app", value: "chat" }
    ]);
    expect(input).toHaveLength(4);
    expect(withClientAppHeader(input, undefined)).toEqual(input);
  });
});
