import { describe, expect, test } from "bun:test";
import {
  getAppleAuthError,
  getAppleAuthorizationNonce,
  isAppleAuthCancellation
} from "./appleOAuth";

describe("shared Apple browser helpers", () => {
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
