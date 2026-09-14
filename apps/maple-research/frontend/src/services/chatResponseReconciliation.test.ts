import { describe, expect, test } from "bun:test";
import {
  classifyChatResponseReconciliation,
  responseIdForChatMessage
} from "./chatResponseReconciliation";

describe("chat response reconciliation", () => {
  test("distinguishes durable completion, other terminal states, and live work", () => {
    expect(classifyChatResponseReconciliation("completed")).toBe("completed");
    expect(classifyChatResponseReconciliation("failed")).toBe("terminal");
    expect(classifyChatResponseReconciliation("cancelled")).toBe("terminal");
    expect(classifyChatResponseReconciliation("incomplete")).toBe("terminal");
    expect(classifyChatResponseReconciliation("queued")).toBe("pending");
    expect(classifyChatResponseReconciliation("in_progress")).toBe("pending");
    expect(classifyChatResponseReconciliation(undefined)).toBe("pending");
  });

  test("recovers only the response linked to the exact current user turn", () => {
    expect(
      responseIdForChatMessage("current-turn", [
        { id: "old-failure", role: "user", response_id: "response-old" },
        { id: "other-tab", role: "user", response_id: "response-other" },
        { id: "current-turn", role: "user", response_id: "response-current" }
      ])
    ).toBe("response-current");
  });

  test("rejects older links, assistant links, and malformed response IDs", () => {
    expect(
      responseIdForChatMessage("current-turn", [
        { id: "old-failure", role: "user", response_id: "response-old" }
      ])
    ).toBeUndefined();
    expect(
      responseIdForChatMessage("current-turn", [
        { id: "current-turn", role: "assistant", response_id: "response-current" },
        { id: "current-turn", role: "user", response_id: 42 }
      ])
    ).toBeUndefined();
  });
});
