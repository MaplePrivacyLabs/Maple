import { describe, expect, test } from "bun:test";
import { getBrowserOAuthCallbackUrl, getNativeOAuthEntryUrl } from "./oauthConfig";

describe("OAuth origin selection", () => {
  test("keeps existing native entry when auth origin is unset or explicitly the apex", () => {
    for (const origin of [undefined, "", "https://trymaple.ai", "https://trymaple.ai/"]) {
      expect(getNativeOAuthEntryUrl(origin)).toBe("https://trymaple.ai/desktop-auth");
    }
  });

  test("uses the permanent entry alias on a configured auth origin", () => {
    expect(getNativeOAuthEntryUrl("https://auth.trymaple.ai")).toBe(
      "https://auth.trymaple.ai/desktop-auth"
    );
  });

  test("keeps every browser provider callback on its initiating origin", () => {
    for (const provider of ["github", "google", "apple"] as const) {
      for (const origin of [
        "https://trymaple.ai",
        "https://app.trymaple.ai",
        "https://auth.trymaple.ai",
        "http://127.0.0.1:35493"
      ]) {
        expect(getBrowserOAuthCallbackUrl(provider, origin)).toBe(
          `${origin}/auth/${provider}/callback`
        );
      }
    }
  });

  test("allows exact loopback HTTP only for development native entry", () => {
    for (const origin of ["http://127.0.0.1:3000", "http://localhost:5173", "http://[::1]:5173"]) {
      expect(getNativeOAuthEntryUrl(origin, true)).toBe(`${origin}/desktop-auth`);
      expect(() => getNativeOAuthEntryUrl(origin, false)).toThrow("HTTPS");
    }
  });

  test("rejects origin configuration with credentials, navigation data, or insecure hosts", () => {
    for (const origin of [
      "https://user:password@auth.trymaple.ai",
      "https://auth.trymaple.ai/start",
      "https://auth.trymaple.ai/./",
      "https://auth.trymaple.ai/%2e/",
      "https://auth.trymaple.ai?next=other",
      "https://auth.trymaple.ai#callback",
      "https://auth.trymaple.ai?",
      "https://auth.trymaple.ai#",
      " https://auth.trymaple.ai",
      "https://auth.trymaple.ai ",
      "https://auth.trymaple.ai\\other",
      "https://auth.trymaple.ai:bad",
      "http://auth.trymaple.ai",
      "http://localhost.example.com",
      "http://127.1",
      "http://2130706433",
      "http://127.0.0.2",
      "http://localhost.",
      "//auth.trymaple.ai",
      "cloud.opensecret.maple://auth",
      "javascript:alert(1)"
    ]) {
      expect(() => getNativeOAuthEntryUrl(origin, true)).toThrow();
    }
  });
});
