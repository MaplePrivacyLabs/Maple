import { describe, expect, test } from "bun:test";
import {
  hostedNativeAppVariant,
  parseDevAuthOrigin,
  nativeAuthOrigin,
  nativeCallbackScheme,
  parseMapleAppVariant
} from "./mapleAppVariant";

const DEV_AUTH_ORIGIN = "https://dev-auth.example.test";

describe("Maple app identity", () => {
  test("defaults to the shipped identity and accepts only explicit variants", () => {
    expect(parseMapleAppVariant(undefined)).toBe("production");
    expect(parseMapleAppVariant("production")).toBe("production");
    expect(parseMapleAppVariant("dev")).toBe("dev");
    for (const invalid of [null, "", "staging", "DEV", ["dev"], "https://example.com"]) {
      expect(() => parseMapleAppVariant(invalid)).toThrow();
    }
  });

  test("separates native callback schemes and hosted sign-in sites", () => {
    expect(nativeCallbackScheme("production")).toBe("cloud.opensecret.maple");
    expect(nativeCallbackScheme("dev")).toBe("cloud.opensecret.maple.dev");
    expect(nativeAuthOrigin("production")).toBe("https://trymaple.ai");
    expect(nativeAuthOrigin("dev", DEV_AUTH_ORIGIN)).toBe(DEV_AUTH_ORIGIN);
  });

  test("requires a canonical HTTPS origin without URL credentials or a path", () => {
    expect(parseDevAuthOrigin(DEV_AUTH_ORIGIN)).toBe(DEV_AUTH_ORIGIN);
    for (const invalid of [
      undefined,
      "",
      "https://user:password@example.test",
      "http://example.test",
      "https://example.test/auth",
      "https://example.test/",
      "https://example.test?callback=x",
      "https://example.test#fragment",
      "https://example.test:443",
      "https://example.test:8443",
      "https://EXAMPLE.test",
      " https://example.test"
    ]) {
      expect(() => parseDevAuthOrigin(invalid)).toThrow();
    }
  });

  test("dev hosted callbacks require the exact dev origin and backend", () => {
    expect(
      hostedNativeAppVariant(
        "dev",
        DEV_AUTH_ORIGIN,
        "https://enclave.secretgpt.ai",
        DEV_AUTH_ORIGIN
      )
    ).toBe("dev");
    for (const origin of [
      "https://trymaple.ai",
      "https://pr-1.maple-ca8.pages.dev",
      `${DEV_AUTH_ORIGIN}.example.com`,
      "http://master.maple-ca8.pages.dev"
    ]) {
      expect(() =>
        hostedNativeAppVariant("dev", origin, "https://enclave.secretgpt.ai", DEV_AUTH_ORIGIN)
      ).toThrow();
    }
    expect(() =>
      hostedNativeAppVariant("dev", DEV_AUTH_ORIGIN, "https://enclave.trymaple.ai", DEV_AUTH_ORIGIN)
    ).toThrow();
    expect(
      hostedNativeAppVariant(undefined, "https://trymaple.ai", "https://enclave.trymaple.ai")
    ).toBe("production");
  });
});
