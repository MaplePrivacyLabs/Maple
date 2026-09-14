import { describe, expect, test } from "bun:test";
import {
  CHAT_ACCOUNT_CREDENTIAL_MISMATCH_CODE,
  ChatAccountCredentialMismatchError,
  assertChatAccountCredential,
  createAccountBoundChatFetch,
  isChatAccountCredentialMismatchError
} from "./chatAccountCredential";

describe("account-bound Chat credentials", () => {
  test("allows a stable V2 principal across calls", async () => {
    let principalId = "user-a";
    const calls: string[] = [];
    const fetch = createAccountBoundChatFetch({
      expectedUserId: "user-a",
      getPrincipalId: () => principalId,
      fetch: async (input) => {
        calls.push(String(input));
        return Response.json({ ok: true });
      }
    });

    await fetch("https://example.test/first");
    principalId = "user-a";
    await fetch("https://example.test/after-refresh");

    expect(calls).toEqual(["https://example.test/first", "https://example.test/after-refresh"]);
  });

  test("blocks a replaced account before invoking the transport", async () => {
    let called = false;
    const fetch = createAccountBoundChatFetch({
      expectedUserId: "user-a",
      getPrincipalId: () => "user-b",
      fetch: async () => {
        called = true;
        return Response.json({ ok: true });
      }
    });

    try {
      await fetch("https://example.test/blocked");
      throw new Error("expected account-bound fetch to reject");
    } catch (error) {
      expect(isChatAccountCredentialMismatchError(error)).toBe(true);
      expect(error).toMatchObject({
        code: CHAT_ACCOUNT_CREDENTIAL_MISMATCH_CODE,
        requestDispatchCode: "opensecret_request_not_dispatched",
        definitelyNotDispatched: true
      });
    }
    expect(called).toBe(false);
  });

  test("guards non-Chat account operations with the same account identity", () => {
    expect(() => assertChatAccountCredential("user-a", () => "user-a")).not.toThrow();
    expect(() => assertChatAccountCredential("user-a", () => "user-b")).toThrow(
      ChatAccountCredentialMismatchError
    );
    expect(() => assertChatAccountCredential(undefined, () => "user-a")).toThrow(
      ChatAccountCredentialMismatchError
    );
  });

  test("recognizes an OpenAI-style wrapped mismatch", () => {
    const mismatch = { code: CHAT_ACCOUNT_CREDENTIAL_MISMATCH_CODE };
    expect(isChatAccountCredentialMismatchError({ cause: mismatch })).toBe(true);
    expect(isChatAccountCredentialMismatchError(new Error("network"))).toBe(false);
  });
});
