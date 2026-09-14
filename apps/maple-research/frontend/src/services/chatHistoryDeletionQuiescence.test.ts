import { describe, expect, test } from "bun:test";
import {
  cancelChatResponseForHistoryDeletion,
  quiesceChatRuntimeRunsForHistoryDeletion,
  settleChatRuntimeAfterHistoryCancellation
} from "./chatHistoryDeletionQuiescence";
import { registerChatCurrentTurn } from "./chatCurrentTurnRegistry";
import { registerUnresolvedChatResponseMessage } from "./chatUnresolvedResponseOwnership";
import {
  ChatRuntimeStore,
  createConversationChatKey,
  type ChatRuntimeKey
} from "./chatRuntimeStore";

function fakeStore() {
  const key = "conversation:active" as ChatRuntimeKey;
  let snapshot: { runToken: number | null; currentResponseId: string | undefined } | undefined = {
    runToken: 1,
    currentResponseId: undefined
  };
  return {
    key,
    setSnapshot(next: typeof snapshot) {
      snapshot = next;
    },
    getActiveRunKeys: () => (snapshot?.runToken === null || !snapshot ? [] : [key]),
    get: () => snapshot,
    setCurrentResponseId: (_key: ChatRuntimeKey, runToken: number, responseId: string) => {
      if (!snapshot || snapshot.runToken !== runToken) return false;
      snapshot = { ...snapshot, currentResponseId: responseId };
      return true;
    },
    cancelRun: () => {
      snapshot = undefined;
    }
  };
}

describe("chat history deletion quiescence", () => {
  test("waits for conversation creation, then cancels the surfaced response", async () => {
    const store = fakeStore();
    const cancelled: string[] = [];
    setTimeout(() => {
      store.setSnapshot({ runToken: 1, currentResponseId: "response-late" });
    }, 5);

    await quiesceChatRuntimeRunsForHistoryDeletion({
      store,
      timeoutMs: 200,
      cancelResponse: async (responseId) => {
        cancelled.push(responseId);
        store.setSnapshot(undefined);
      }
    });

    expect(cancelled).toEqual(["response-late"]);
  });

  test("fails closed instead of deleting while a create request remains unresolved", async () => {
    const store = fakeStore();

    await expect(
      quiesceChatRuntimeRunsForHistoryDeletion({
        store,
        timeoutMs: 1,
        cancelResponse: async () => undefined
      })
    ).rejects.toThrow("Timed out waiting for active chat requests to stop");
  });

  test("allows a naturally completed run to settle after cancellation loses the race", async () => {
    const store = fakeStore();
    store.setSnapshot({ runToken: 1, currentResponseId: "response-completing" });

    await quiesceChatRuntimeRunsForHistoryDeletion({
      store,
      timeoutMs: 200,
      cancelResponse: async () => {
        setTimeout(() => store.setSnapshot(undefined), 5);
        throw new Error("response is already terminal");
      }
    });

    expect(store.getActiveRunKeys()).toEqual([]);
  });

  test("still fails closed when cancellation rejects and the run stays active", async () => {
    const store = fakeStore();
    store.setSnapshot({ runToken: 1, currentResponseId: "response-stuck" });

    await expect(
      quiesceChatRuntimeRunsForHistoryDeletion({
        store,
        timeoutMs: 1,
        cancelResponse: async () => {
          throw new Error("cancel failed");
        }
      })
    ).rejects.toThrow("Timed out waiting for active chat requests to stop");
  });

  test("retries a rejected cancellation while response ownership remains", async () => {
    const store = fakeStore();
    store.setSnapshot({ runToken: 1, currentResponseId: "response-retry" });
    let attempts = 0;

    await quiesceChatRuntimeRunsForHistoryDeletion({
      store,
      timeoutMs: 750,
      cancelResponse: async () => {
        attempts += 1;
        if (attempts === 1) throw new Error("transient cancellation error");
        store.setSnapshot(undefined);
      }
    });

    expect(attempts).toBe(2);
  });

  test("bounds a cancellation request that never settles", async () => {
    const store = fakeStore();
    store.setSnapshot({ runToken: 1, currentResponseId: "response-hung" });

    await expect(
      quiesceChatRuntimeRunsForHistoryDeletion({
        store,
        timeoutMs: 5,
        cancelResponse: () => new Promise(() => undefined)
      })
    ).rejects.toThrow("Timed out waiting for active chat requests to stop");
  });

  test("quiesces only exact target keys and leaves unrelated runs active", async () => {
    const target = "conversation:target" as ChatRuntimeKey;
    const unrelated = "conversation:unrelated" as ChatRuntimeKey;
    const snapshots = new Map([
      [target, { runToken: 1, currentResponseId: "response-target" }],
      [unrelated, { runToken: 2, currentResponseId: "response-unrelated" }]
    ]);
    const store = {
      getActiveRunKeys: () => Array.from(snapshots.keys()),
      get: (key: ChatRuntimeKey) => snapshots.get(key),
      resolveKey: (key: ChatRuntimeKey) => key,
      cancelRun: (key: ChatRuntimeKey) => snapshots.delete(key)
    };

    await quiesceChatRuntimeRunsForHistoryDeletion({
      store,
      keys: [target],
      timeoutMs: 100,
      cancelResponse: async (responseId) => {
        expect(responseId).toBe("response-target");
        snapshots.delete(target);
      }
    });

    expect(store.getActiveRunKeys()).toEqual([unrelated]);
  });

  test("includes active draft runs owned by a deleted project group", async () => {
    const draft = "draft:project" as ChatRuntimeKey;
    const unrelated = "draft:other" as ChatRuntimeKey;
    const snapshots = new Map([
      [draft, { runToken: 1, currentResponseId: "response-project" }],
      [unrelated, { runToken: 2, currentResponseId: "response-other" }]
    ]);
    const store = {
      getActiveRunKeys: () => Array.from(snapshots.keys()),
      get: (key: ChatRuntimeKey) => snapshots.get(key),
      getActiveRunGroupId: (key: ChatRuntimeKey) =>
        key === draft ? "project-delete" : "project-other",
      cancelRun: (key: ChatRuntimeKey) => snapshots.delete(key)
    };

    await quiesceChatRuntimeRunsForHistoryDeletion({
      store,
      activityGroupId: "project-delete",
      timeoutMs: 100,
      cancelResponse: async () => {
        snapshots.delete(draft);
      }
    });

    expect(store.getActiveRunKeys()).toEqual([unrelated]);
  });

  test("settles an offscreen detached run after durable server cancellation", async () => {
    const store = fakeStore();
    store.setSnapshot({ runToken: 7, currentResponseId: "response-detached" });
    const cancelled: string[] = [];

    await quiesceChatRuntimeRunsForHistoryDeletion({
      store,
      timeoutMs: 100,
      cancelResponse: async (responseId) => {
        cancelled.push(responseId);
      }
    });

    expect(cancelled).toEqual(["response-detached"]);
    expect(store.getActiveRunKeys()).toEqual([]);
  });

  test("recovers an offscreen response by the exact current optimistic message before deletion", async () => {
    const store = fakeStore();
    registerUnresolvedChatResponseMessage(store, 1, "message-current");
    const retrieved: Array<{ messageId: string; conversationId: string }> = [];
    const cancelled: string[] = [];

    await quiesceChatRuntimeRunsForHistoryDeletion({
      store,
      timeoutMs: 100,
      responseOwnershipClient: {
        conversations: {
          items: {
            retrieve: async (messageId, query) => {
              retrieved.push({ messageId, conversationId: query.conversation_id });
              return {
                id: "message-current",
                role: "user",
                response_id: "response-current"
              };
            }
          }
        }
      },
      cancelResponse: async (responseId) => {
        cancelled.push(responseId);
      }
    });

    expect(retrieved).toEqual([{ messageId: "message-current", conversationId: "active" }]);
    expect(cancelled).toEqual(["response-current"]);
    expect(store.getActiveRunKeys()).toEqual([]);
  });

  test("settles an offscreen run that became terminal before cancellation", async () => {
    const store = fakeStore();
    store.setSnapshot({ runToken: 8, currentResponseId: "response-completed" });
    let cancelAttempts = 0;
    let retrieveAttempts = 0;
    const client = {
      responses: {
        cancel: async () => {
          cancelAttempts += 1;
          throw Object.assign(new Error("response is already terminal"), { status: 400 });
        },
        retrieve: async () => {
          retrieveAttempts += 1;
          return { status: "completed" };
        }
      }
    };

    await quiesceChatRuntimeRunsForHistoryDeletion({
      store,
      timeoutMs: 100,
      cancelResponse: (responseId) => cancelChatResponseForHistoryDeletion(client, responseId)
    });

    expect(cancelAttempts).toBe(1);
    expect(retrieveAttempts).toBe(1);
    expect(store.getActiveRunKeys()).toEqual([]);
  });

  test("does not accept a terminal retrieval after an ambiguous cancel 503", async () => {
    let retrieveAttempts = 0;
    const cancellationError = Object.assign(new Error("cancel quiescence timed out"), {
      status: 503
    });
    const client = {
      responses: {
        cancel: async () => {
          throw cancellationError;
        },
        retrieve: async () => {
          retrieveAttempts += 1;
          return { status: "cancelled" };
        }
      }
    };

    await expect(
      cancelChatResponseForHistoryDeletion(client, "response-not-quiescent")
    ).rejects.toBe(cancellationError);
    expect(retrieveAttempts).toBe(0);
  });

  test("locally settles preparation that has not started a server request", async () => {
    const store = fakeStore();
    let restored = false;
    registerChatCurrentTurn(store, 1, {
      responseRequestStarted: () => false,
      serverRequestInFlight: () => false,
      restoreBeforeRequest: () => {
        restored = true;
        return true;
      }
    });

    await quiesceChatRuntimeRunsForHistoryDeletion({
      store,
      timeoutMs: 100,
      cancelResponse: async () => {
        throw new Error("no server response should be cancelled");
      }
    });

    expect(restored).toBe(true);
    expect(store.getActiveRunKeys()).toEqual([]);
  });

  test("does not locally settle while conversation creation is in flight", async () => {
    const store = fakeStore();
    let restored = false;
    registerChatCurrentTurn(store, 1, {
      responseRequestStarted: () => false,
      serverRequestInFlight: () => true,
      restoreBeforeRequest: () => {
        restored = true;
        return true;
      }
    });

    await expect(
      quiesceChatRuntimeRunsForHistoryDeletion({
        store,
        timeoutMs: 2,
        cancelResponse: async () => undefined
      })
    ).rejects.toThrow("Timed out waiting for active chat requests to stop");

    expect(restored).toBe(false);
    expect(store.getActiveRunKeys()).toHaveLength(1);
  });

  test("marks partial items incomplete when cancellation succeeds but deletion later fails", () => {
    const store = new ChatRuntimeStore<null, { id: string; status: string }, { input: string }>({
      createComposer: () => ({ input: "" })
    });
    const key = createConversationChatKey("cancelled-before-delete");
    store.select(key);
    store.update(key, (snapshot) => ({
      ...snapshot,
      messages: [
        { id: "user", status: "completed" },
        { id: "assistant", status: "streaming" }
      ]
    }));
    const run = store.beginRun(key);
    store.setCurrentResponseId(key, run.token, "response-cancelled");

    expect(settleChatRuntimeAfterHistoryCancellation(store, key, run.token)).toBe(true);
    expect(store.get(key)).toMatchObject({ isGenerating: false, currentResponseId: undefined });
    expect(store.get(key)?.messages).toEqual([
      { id: "user", status: "completed" },
      { id: "assistant", status: "incomplete" }
    ]);
  });
});
