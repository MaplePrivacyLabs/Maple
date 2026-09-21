import { describe, expect, test } from "bun:test";
import { getBrowserOAuthCallbackUrl } from "./oauthConfig";

describe("OAuth origin selection", () => {
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
      expect(() => getBrowserOAuthCallbackUrl("github", origin)).toThrow();
    }
  });
});
