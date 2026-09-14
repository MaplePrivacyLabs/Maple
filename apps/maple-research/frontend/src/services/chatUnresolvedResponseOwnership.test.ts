import { describe, expect, test } from "bun:test";
import {
  clearUnresolvedChatResponseMessage,
  getUnresolvedChatResponseMessage,
  registerUnresolvedChatResponseMessage
} from "./chatUnresolvedResponseOwnership";

describe("chat unresolved response ownership", () => {
  test("tracks exactly one optimistic message per active run", () => {
    const store = {};
    registerUnresolvedChatResponseMessage(store, 7, "current-turn");

    expect(getUnresolvedChatResponseMessage(store, 7)).toBe("current-turn");
    expect(getUnresolvedChatResponseMessage(store, 8)).toBeUndefined();
  });

  test("a stale clear cannot remove replacement ownership", () => {
    const store = {};
    registerUnresolvedChatResponseMessage(store, 9, "first-turn");
    registerUnresolvedChatResponseMessage(store, 9, "replacement-turn");

    clearUnresolvedChatResponseMessage(store, 9, "first-turn");
    expect(getUnresolvedChatResponseMessage(store, 9)).toBe("replacement-turn");

    clearUnresolvedChatResponseMessage(store, 9, "replacement-turn");
    expect(getUnresolvedChatResponseMessage(store, 9)).toBeUndefined();
  });

  test("isolates stores with identical run tokens", () => {
    const firstStore = {};
    const secondStore = {};
    registerUnresolvedChatResponseMessage(firstStore, 1, "first");
    registerUnresolvedChatResponseMessage(secondStore, 1, "second");

    clearUnresolvedChatResponseMessage(firstStore, 1);
    expect(getUnresolvedChatResponseMessage(firstStore, 1)).toBeUndefined();
    expect(getUnresolvedChatResponseMessage(secondStore, 1)).toBe("second");
  });
});
