import { describe, expect, test } from "bun:test";
import {
  clearChatRunQueueHalt,
  isChatRunQueueHaltRequested,
  requestChatRunQueueHalt
} from "./chatRunQueueHalt";

describe("chat run queue halt", () => {
  test("keeps Stop intent scoped to one store and outer run token", () => {
    const firstStore = {};
    const secondStore = {};

    requestChatRunQueueHalt(firstStore, 7);
    expect(isChatRunQueueHaltRequested(firstStore, 7)).toBe(true);
    expect(isChatRunQueueHaltRequested(firstStore, 8)).toBe(false);
    expect(isChatRunQueueHaltRequested(secondStore, 7)).toBe(false);

    clearChatRunQueueHalt(firstStore, 7);
    expect(isChatRunQueueHaltRequested(firstStore, 7)).toBe(false);
  });

  test("clearing one halted run preserves another", () => {
    const store = {};
    requestChatRunQueueHalt(store, 1);
    requestChatRunQueueHalt(store, 2);

    clearChatRunQueueHalt(store, 1);
    expect(isChatRunQueueHaltRequested(store, 1)).toBe(false);
    expect(isChatRunQueueHaltRequested(store, 2)).toBe(true);
  });
});
