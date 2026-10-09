import { describe, expect, test } from "bun:test";
import {
  getAppleAuthError,
  getAppleAuthorizationClientId,
  getAppleAuthorizationNonce,
  isAppleAuthCancellation
} from "./appleOAuth";

describe("shared Apple browser helpers", () => {
  test("uses the exact Services ID selected by the backend for either environment", () => {
    for (const clientId of [
      "cloud.opensecret.maple.services",
      "cloud.opensecret.maple.dev.services"
    ]) {
      expect(
        getAppleAuthorizationClientId(
          `https://appleid.apple.com/auth/authorize?client_id=${clientId}`
        )
      ).toBe(clientId);
    }
  });

  test("rejects invalid provider URLs and ambiguous or malformed client IDs", () => {
    const valid =
      "https://appleid.apple.com/auth/authorize?client_id=cloud.opensecret.maple.services";
    for (const authUrl of [
      "not a URL",
      valid.replace("https:", "http:"),
      valid.replace("appleid.apple.com", "appleid.apple.com.example.test"),
      valid.replace("appleid.apple.com", "user:pass@appleid.apple.com"),
      valid.replace("appleid.apple.com", "appleid.apple.com:8443"),
      valid.replace("/auth/authorize", "/auth/token"),
      `${valid}#fragment`,
      "https://appleid.apple.com/auth/authorize",
      "https://appleid.apple.com/auth/authorize?client_id=",
      `${valid}&client_id=cloud.opensecret.maple.services`,
      `${valid}&client_id=cloud.opensecret.maple.dev.services`,
      `${valid}%20`,
      valid.replace("cloud.opensecret.maple.services", "cloud..maple.services"),
      valid.replace("cloud.opensecret.maple.services", "cloud/maple")
    ]) {
      expect(() => getAppleAuthorizationClientId(authUrl)).toThrow("valid client ID");
    }
  });

  test("preserves cancellation codes and provides a useful popup-blocked retry", () => {
    for (const code of ["user_cancelled_authorize", "popup_closed_by_user"]) {
      expect(isAppleAuthCancellation(getAppleAuthError({ error: code }))).toBe(true);
    }
    for (const code of ["popup_blocked_by_browser", "popup_blocked"]) {
      const error = getAppleAuthError({ error: code });
      expect(error.message).toContain("Allow popups for this site");
      expect(isAppleAuthCancellation(error)).toBe(false);
    }
    expect(getAppleAuthError({ error: "" }).message).toBe("Apple authentication failed");
  });

  test("accepts only one canonical backend nonce", () => {
    const nonce = "12".repeat(32);
    expect(
      getAppleAuthorizationNonce(`https://appleid.apple.com/auth/authorize?nonce=${nonce}`)
    ).toBe(nonce);
    for (const suffix of ["", "?nonce=", "?nonce=ABC", `?nonce=${nonce}&nonce=${nonce}`]) {
      expect(() =>
        getAppleAuthorizationNonce(`https://appleid.apple.com/auth/authorize${suffix}`)
      ).toThrow("valid nonce");
    }
  });
});
