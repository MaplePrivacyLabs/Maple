import { describe, expect, test } from "bun:test";
import {
  getRegisteredChatCurrentTurnPayloads,
  registeredChatTurnCanSettleLocallyForDeletion,
  registerChatCurrentTurn,
  restoreRegisteredChatTurnBeforeRequest
} from "./chatCurrentTurnRegistry";

describe("chat current turn registry", () => {
  test("restores only a turn that has not started its Responses request", () => {
    const store = {};
    let requestStarted = false;
    const restored: string[] = [];
    const unregister = registerChatCurrentTurn(store, 7, {
      responseRequestStarted: () => requestStarted,
      restoreBeforeRequest: (message) => {
        restored.push(message);
        return true;
      }
    });

    expect(restoreRegisteredChatTurnBeforeRequest(store, 7, "stopped")).toBe(true);
    expect(restored).toEqual(["stopped"]);

    requestStarted = true;
    expect(restoreRegisteredChatTurnBeforeRequest(store, 7, "too late")).toBe(false);
    expect(restored).toEqual(["stopped"]);

    unregister();
    expect(restoreRegisteredChatTurnBeforeRequest(store, 7, "missing")).toBe(false);
  });

  test("an older unregister cannot remove a replacement control", () => {
    const store = {};
    const first = registerChatCurrentTurn(store, 9, {
      responseRequestStarted: () => false,
      restoreBeforeRequest: () => false
    });
    let restored = false;
    registerChatCurrentTurn(store, 9, {
      responseRequestStarted: () => false,
      restoreBeforeRequest: () => (restored = true),
      retainedPayload: { queueId: "replacement" },
      countsTowardQueueLimit: true
    });

    first();
    expect(restoreRegisteredChatTurnBeforeRequest(store, 9, "replacement")).toBe(true);
    expect(restored).toBe(true);
    expect(getRegisteredChatCurrentTurnPayloads(store)).toEqual([
      { payload: { queueId: "replacement" }, countsTowardQueueLimit: true }
    ]);
  });

  test("restores for Stop but does not authorize deletion settlement during conversation creation", () => {
    const store = {};
    let createInFlight = true;
    const restored: string[] = [];
    registerChatCurrentTurn(store, 10, {
      responseRequestStarted: () => false,
      serverRequestInFlight: () => createInFlight,
      restoreBeforeRequest: (message) => {
        restored.push(message);
        return true;
      }
    });

    expect(restoreRegisteredChatTurnBeforeRequest(store, 10, "stop now")).toBe(true);
    expect(registeredChatTurnCanSettleLocallyForDeletion(store, 10)).toBe(false);
    createInFlight = false;
    expect(restoreRegisteredChatTurnBeforeRequest(store, 10, "safe now")).toBe(true);
    expect(registeredChatTurnCanSettleLocallyForDeletion(store, 10)).toBe(true);
    expect(restored).toEqual(["stop now", "safe now"]);
  });

  test("stops retaining a payload as soon as ownership transfers back to a composer", () => {
    const store = {};
    let retainsPayload = true;
    registerChatCurrentTurn(store, 11, {
      responseRequestStarted: () => false,
      restoreBeforeRequest: () => false,
      retainedPayload: { queueId: "current" },
      retainsPayload: () => retainsPayload,
      countsTowardQueueLimit: true
    });

    expect(getRegisteredChatCurrentTurnPayloads(store)).toHaveLength(1);
    retainsPayload = false;
    expect(getRegisteredChatCurrentTurnPayloads(store)).toEqual([]);
  });
});
