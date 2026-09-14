import { describe, expect, test } from "bun:test";
import {
  ChatRuntimeStore,
  createChatDraftKey,
  createConversationChatKey,
  type ChatRuntimeRunUpdater,
  type ChatRuntimeSnapshot
} from "./chatRuntimeStore";
import { chatCursorAfterSendFailure } from "./chatSendFailureRecovery";

type Conversation = { id: string; title: string };
type Message = { id: string; text: string; status: "streaming" | "completed" };
type Composer = { input: string; attachmentIds: string[] };
type Snapshot = ChatRuntimeSnapshot<Conversation, Message, Composer>;

const A = createConversationChatKey("a");
const B = createConversationChatKey("b");

function createStore(
  options: {
    maxInactiveCompletedEntries?: number;
    canEvict?: (snapshot: Snapshot) => boolean;
    disposeEntry?: (snapshot: Snapshot, reason: "evicted" | "deleted" | "disposed") => void;
  } = {}
) {
  return new ChatRuntimeStore<Conversation, Message, Composer>({
    createComposer: () => ({ input: "", attachmentIds: [] }),
    ...options
  });
}

function streamingMessage(id: string, text: string): Message {
  return { id, text, status: "streaming" };
}

describe("ChatRuntimeStore", () => {
  test("keeps concurrent A and B runs isolated while A receives offscreen updates", () => {
    const store = createStore();
    store.select(A, {
      conversation: { id: "a", title: "Chat A" },
      messages: [{ id: "a-user", text: "Question A", status: "completed" }],
      lastSeenItemId: "a-user",
      historyLoaded: true
    });

    const runA = store.beginRun(A);
    store.setCurrentResponseId(A, runA.token, "response-a");
    store.setAssistantStreaming(A, runA.token, true);
    expect(
      store.updateForRun(A, runA.token, (snapshot) => ({
        ...snapshot,
        messages: [...snapshot.messages, streamingMessage("a-assistant", "one ")]
      }))
    ).toBe(true);

    store.select(B, {
      conversation: { id: "b", title: "Chat B" },
      messages: [{ id: "b-user", text: "Question B", status: "completed" }],
      historyLoaded: true
    });
    const runB = store.beginRun(B);
    store.setCurrentResponseId(B, runB.token, "response-b");
    store.setAssistantStreaming(B, runB.token, true);
    store.updateForRun(B, runB.token, (snapshot) => ({
      ...snapshot,
      messages: [...snapshot.messages, streamingMessage("b-assistant", "answer B")]
    }));

    // A's HTTP iterator can continue dispatching while B is the selected chat.
    store.updateForRun(A, runA.token, (snapshot) => ({
      ...snapshot,
      messages: snapshot.messages.map((message) =>
        message.id === "a-assistant" ? { ...message, text: `${message.text}two` } : message
      ),
      lastSeenItemId: "a-assistant"
    }));

    expect(store.getActiveKey()).toBe(B);
    expect(store.getActive()?.messages.at(-1)?.text).toBe("answer B");
    expect(store.get(A)?.messages.at(-1)?.text).toBe("one two");
    expect(store.get(A)?.isGenerating).toBe(true);
    expect(store.get(B)?.isGenerating).toBe(true);
    expect(runA.signal.aborted).toBe(false);
    expect(runB.signal.aborted).toBe(false);

    store.finishRun(B, runB.token, (snapshot) => ({
      ...snapshot,
      messages: snapshot.messages.map((message) => ({ ...message, status: "completed" }))
    }));
    store.select(A);

    expect(store.getActive()?.messages.at(-1)?.text).toBe("one two");
    expect(store.getActive()?.assistantStreaming).toBe(true);
    expect(store.getActive()?.currentResponseId).toBe("response-a");
    expect(store.get(B)?.isGenerating).toBe(false);
  });

  test("finishes A offscreen without changing selected running B", () => {
    const store = createStore();
    store.select(A, {
      messages: [streamingMessage("a-assistant", "answer A")]
    });
    const runA = store.beginRun(A);
    store.setCurrentResponseId(A, runA.token, "response-a");
    store.setAssistantStreaming(A, runA.token, true);

    store.select(B, {
      messages: [streamingMessage("b-assistant", "answer B")]
    });
    const runB = store.beginRun(B);
    store.setCurrentResponseId(B, runB.token, "response-b");
    store.setAssistantStreaming(B, runB.token, true);
    const selectedBSnapshot = store.get(B);

    expect(
      store.finishRun(A, runA.token, (snapshot) => ({
        ...snapshot,
        messages: snapshot.messages.map((message) => ({
          ...message,
          status: "completed"
        }))
      }))
    ).toBe(true);

    expect(store.getActiveKey()).toBe(B);
    expect(store.get(B)).toBe(selectedBSnapshot);
    expect(store.get(B)).toMatchObject({
      isGenerating: true,
      assistantStreaming: true,
      currentResponseId: "response-b",
      runToken: runB.token
    });
    expect(runB.signal.aborted).toBe(false);
    expect(store.get(A)).toMatchObject({
      isGenerating: false,
      assistantStreaming: false,
      currentResponseId: undefined,
      runToken: null
    });
    expect(store.get(A)?.messages.at(-1)?.status).toBe("completed");
  });

  test("owns composer state independently for every chat", () => {
    const store = createStore();
    store.select(A);
    store.update(A, (snapshot) => ({
      ...snapshot,
      composer: { input: "draft A", attachmentIds: ["a.png"] }
    }));

    store.select(B);
    store.update(B, (snapshot) => ({
      ...snapshot,
      composer: { input: "draft B", attachmentIds: ["b.pdf"] }
    }));
    store.update(A, (snapshot) => ({
      ...snapshot,
      composer: { ...snapshot.composer, input: "updated A while offscreen" }
    }));

    expect(store.getActive()?.composer).toEqual({ input: "draft B", attachmentIds: ["b.pdf"] });
    expect(store.get(A)?.composer).toEqual({
      input: "updated A while offscreen",
      attachmentIds: ["a.png"]
    });
    const snapshots = store.getSnapshots();
    expect(Object.isFrozen(snapshots)).toBe(true);
    expect(new Set(snapshots.map((snapshot) => snapshot.key))).toEqual(new Set([A, B]));

    store.select(A);
    expect(store.getActive()?.composer.input).toBe("updated A while offscreen");
  });

  test("rekeys a draft safely without stealing focus and keeps the old key as an alias", () => {
    const store = createStore();
    const draft = createChatDraftKey("pending-a");
    const conversation = createConversationChatKey("created-a");
    store.select(A);
    store.ensure(draft, {
      composer: { input: "draft prompt", attachmentIds: ["image"] },
      messages: [streamingMessage("local-user", "draft prompt")]
    });
    const run = store.beginRun(draft);
    store.setCurrentResponseId(draft, run.token, "created-response");

    expect(store.rekey(draft, conversation, run.token)).toBe(conversation);
    expect(store.getActiveKey()).toBe(A);
    expect(store.resolveKey(draft)).toBe(conversation);
    expect(store.get(draft)).toBe(store.get(conversation));
    expect(store.get(conversation)?.composer.input).toBe("draft prompt");
    expect(store.isRunCurrent(draft, run.token)).toBe(true);

    expect(
      store.updateForRun(draft, run.token, (snapshot) => ({
        ...snapshot,
        conversation: { id: "created-a", title: "Created" },
        messages: [...snapshot.messages, streamingMessage("assistant", "still streaming")]
      }))
    ).toBe(true);
    expect(store.get(conversation)?.messages.at(-1)?.text).toBe("still streaming");

    store.select(draft);
    expect(store.getActiveKey()).toBe(conversation);
    const activeDraft = createChatDraftKey("active");
    store.select(activeDraft);
    const activeRun = store.beginRun(activeDraft);
    const activeConversation = createConversationChatKey("active-created");
    store.rekey(activeDraft, activeConversation, activeRun.token);
    expect(store.getActiveKey()).toBe(activeConversation);
  });

  test("a stale run cannot rekey replacement state after cancellation", () => {
    const store = createStore();
    const draft = createChatDraftKey("stale-create");
    const staleConversation = createConversationChatKey("stale-created");
    const currentConversation = createConversationChatKey("current-created");
    store.select(draft);
    const staleRun = store.beginRun(draft);
    store.cancelRun(draft, staleRun.token);
    const replacement = store.beginRun(draft);

    expect(store.rekey(draft, staleConversation, staleRun.token)).toBeNull();
    expect(store.resolveKey(draft)).toBe(draft);
    expect(store.getActiveKey()).toBe(draft);
    expect(store.isRunCurrent(draft, replacement.token)).toBe(true);
    expect(replacement.signal.aborted).toBe(false);

    expect(store.rekey(draft, currentConversation, replacement.token)).toBe(currentConversation);
    expect(store.getActiveKey()).toBe(currentConversation);
  });

  test("stale adoption leaves an independently discovered destination untouched", () => {
    const store = createStore();
    const draft = createChatDraftKey("stale-source");
    const conversation = createConversationChatKey("independently-discovered");
    store.select(draft, {
      messages: [{ id: "source-user", text: "source prompt", status: "completed" }]
    });
    const sourceRun = store.beginRun(draft);
    store.select(conversation, {
      conversation: { id: "independently-discovered", title: "Discovered elsewhere" },
      messages: [{ id: "remote-user", text: "remote prompt", status: "completed" }],
      historyLoaded: true
    });
    const destinationBefore = store.get(conversation);
    store.cancelRun(draft, sourceRun.token);

    const result = store.rekeyRunAdoptingIdleDestination(
      draft,
      conversation,
      sourceRun.token,
      () => {
        throw new Error("a stale source must not merge the destination");
      }
    );

    expect(result).toEqual({ status: "source_stale" });
    expect(store.get(conversation)).toBe(destinationBefore);
    expect(store.resolveKey(draft)).toBe(draft);
    expect(store.get(draft)?.messages[0]?.text).toBe("source prompt");
    expect(sourceRun.signal.aborted).toBe(true);
  });

  test("atomically adopts an idle selected destination while the source run continues", () => {
    const disposed: Array<{ key: string; reason: string }> = [];
    const store = createStore({
      disposeEntry: (snapshot, reason) => disposed.push({ key: snapshot.key, reason })
    });
    const draft = createChatDraftKey("server-create-race");
    const conversation = createConversationChatKey("discovered-before-create-returned");
    store.select(draft, {
      messages: [{ id: "optimistic-user", text: "source prompt", status: "completed" }],
      composer: { input: "", attachmentIds: [] }
    });
    const sourceRun = store.beginRun(draft);
    store.setCurrentResponseId(draft, sourceRun.token, "source-response");

    store.select(conversation, {
      conversation: { id: "discovered-before-create-returned", title: "New Conversation" },
      messages: [{ id: "server-history", text: "loaded first", status: "completed" }],
      composer: { input: "selected destination draft", attachmentIds: ["destination.png"] },
      historyLoaded: true
    });
    const observedDestinationSnapshots: Array<Snapshot | undefined> = [];
    const unsubscribe = store.subscribeKey(conversation, () => {
      observedDestinationSnapshots.push(store.get(conversation));
    });

    const result = store.rekeyRunAdoptingIdleDestination(
      draft,
      conversation,
      sourceRun.token,
      (source, destination) => ({
        conversation: destination.conversation ?? source.conversation,
        messages: [...destination.messages, ...source.messages],
        composer: destination.composer,
        currentResponseId: source.currentResponseId,
        error: source.error,
        lastSeenItemId: source.lastSeenItemId ?? destination.lastSeenItemId,
        assistantStreaming: source.assistantStreaming,
        historyLoaded: source.historyLoaded || destination.historyLoaded
      })
    );

    expect(result).toEqual({
      status: "migrated",
      key: conversation,
      adoptedExistingDestination: true,
      destinationWasSelected: true
    });
    expect(observedDestinationSnapshots).toHaveLength(1);
    expect(observedDestinationSnapshots[0]).toBeDefined();
    expect(disposed).toEqual([]);
    expect(store.getActiveKey()).toBe(conversation);
    expect(store.resolveKey(draft)).toBe(conversation);
    expect(store.isRunCurrent(conversation, sourceRun.token)).toBe(true);
    expect(sourceRun.signal.aborted).toBe(false);
    expect(store.get(conversation)).toMatchObject({
      isGenerating: true,
      currentResponseId: "source-response",
      historyLoaded: true,
      composer: {
        input: "selected destination draft",
        attachmentIds: ["destination.png"]
      }
    });
    expect(store.get(conversation)?.messages.map((message) => message.id)).toEqual([
      "server-history",
      "optimistic-user"
    ]);
    unsubscribe();
  });

  test("fails closed when the destination owns a run and leaves both prompts recoverable", () => {
    const store = createStore();
    const draft = createChatDraftKey("source-with-prompt");
    const conversation = createConversationChatKey("busy-destination");
    store.select(draft, {
      messages: [{ id: "source-user", text: "source prompt", status: "completed" }]
    });
    const sourceRun = store.beginRun(draft);
    store.select(conversation, {
      messages: [{ id: "destination-user", text: "destination prompt", status: "completed" }]
    });
    const destinationRun = store.beginRun(conversation);
    const destinationBeforeCollision = store.get(conversation);

    const result = store.rekeyRunAdoptingIdleDestination(
      draft,
      conversation,
      sourceRun.token,
      (source) => source
    );

    expect(result).toEqual({ status: "destination_active", key: conversation });
    expect(store.resolveKey(draft)).toBe(draft);
    expect(store.get(conversation)).toBe(destinationBeforeCollision);
    expect(store.isRunCurrent(draft, sourceRun.token)).toBe(true);
    expect(store.isRunCurrent(conversation, destinationRun.token)).toBe(true);
    expect(sourceRun.signal.aborted).toBe(false);
    expect(destinationRun.signal.aborted).toBe(false);

    // The caller can restore the source prompt in its history-addressable draft
    // without mutating the selected destination's run ownership.
    store.updateForRun(draft, sourceRun.token, (snapshot) => ({
      ...snapshot,
      messages: [],
      composer: { ...snapshot.composer, input: "source prompt" },
      error: "Source prompt restored"
    }));
    store.finishRun(draft, sourceRun.token);
    expect(store.get(conversation)).toMatchObject({
      isGenerating: true,
      runToken: destinationRun.token,
      composer: { input: "" }
    });
    expect(store.get(draft)).toMatchObject({
      isGenerating: false,
      runToken: null,
      composer: { input: "source prompt" },
      error: "Source prompt restored",
      messages: []
    });
  });

  test("keeps a draft-key subscriber live through rekey and conversation-key updates", () => {
    const store = createStore();
    const draft = createChatDraftKey("subscribed-draft");
    const conversation = createConversationChatKey("subscribed-conversation");
    store.select(draft);
    const run = store.beginRun(draft);
    let notifications = 0;
    const unsubscribe = store.subscribeKey(draft, () => {
      notifications += 1;
    });

    expect(store.rekey(draft, conversation, run.token)).toBe(conversation);
    expect(notifications).toBe(1);
    expect(store.get(draft)).toBe(store.get(conversation));

    expect(
      store.updateForRun(conversation, run.token, (snapshot) => ({
        ...snapshot,
        messages: [...snapshot.messages, streamingMessage("assistant", "continued")]
      }))
    ).toBe(true);

    expect(notifications).toBe(2);
    expect(store.get(draft)).toBe(store.get(conversation));
    expect(store.get(draft)?.messages.at(-1)?.text).toBe("continued");
    unsubscribe();
  });

  test("cancels only the keyed run and returns its owned response ID and controller", () => {
    const store = createStore();
    store.select(A);
    const runA = store.beginRun(A);
    store.setCurrentResponseId(A, runA.token, "response-a");
    store.setAssistantStreaming(A, runA.token, true);

    store.select(B);
    const runB = store.beginRun(B);
    store.setCurrentResponseId(B, runB.token, "response-b");
    store.setAssistantStreaming(B, runB.token, true);

    const cancelled = store.cancelRun(A, runA.token);

    expect(cancelled).toEqual({
      key: A,
      token: runA.token,
      responseId: "response-a",
      controller: runA.controller
    });
    expect(runA.signal.aborted).toBe(true);
    expect(store.get(A)).toMatchObject({
      isGenerating: false,
      assistantStreaming: false,
      currentResponseId: undefined,
      runToken: null
    });

    expect(runB.signal.aborted).toBe(false);
    expect(store.get(B)).toMatchObject({
      isGenerating: true,
      assistantStreaming: true,
      currentResponseId: "response-b",
      runToken: runB.token
    });
  });

  test("can reconcile durable completion while aborting the stale local stream", () => {
    const store = createStore();
    store.select(A);
    const run = store.beginRun(A);
    store.updateForRun(A, run.token, (snapshot) => ({
      ...snapshot,
      lastSeenItemId: "persisted-user"
    }));
    store.setCurrentResponseId(A, run.token, "completed-response");
    store.setAssistantStreaming(A, run.token, true);

    expect(
      store.completeRunAndAbort(A, run.token, (snapshot) => ({
        ...snapshot,
        lastSeenItemId: chatCursorAfterSendFailure({
          currentCursor: snapshot.lastSeenItemId,
          optimisticMessageId: "persisted-user",
          previousCursor: undefined,
          responseCreated: Boolean(snapshot.currentResponseId)
        })
      }))
    ).toBe(true);
    expect(run.signal.aborted).toBe(true);
    expect(store.get(A)).toMatchObject({
      isGenerating: false,
      assistantStreaming: false,
      currentResponseId: undefined,
      lastSeenItemId: "persisted-user",
      runToken: null
    });
  });

  test("stale run updates and completion cannot clear a replacement run", () => {
    const store = createStore();
    store.select(A);
    const first = store.beginRun(A);
    store.setCurrentResponseId(A, first.token, "old-response");
    expect(store.finishRun(A, first.token)).toBe(true);

    const replacement = store.beginRun(A);
    store.setCurrentResponseId(A, replacement.token, "new-response");
    store.setAssistantStreaming(A, replacement.token, true);

    expect(
      store.updateForRun(A, first.token, (snapshot) => ({
        ...snapshot,
        error: "stale delta"
      }))
    ).toBe(false);
    expect(store.finishRun(A, first.token)).toBe(false);
    expect(store.get(A)).toMatchObject({
      isGenerating: true,
      assistantStreaming: true,
      currentResponseId: "new-response",
      error: null,
      runToken: replacement.token
    });

    expect(store.finishRun(A, replacement.token)).toBe(true);
    expect(store.get(A)).toMatchObject({
      isGenerating: false,
      assistantStreaming: false,
      currentResponseId: undefined,
      runToken: null
    });
  });

  test("a stale abort callback cannot cancel the replacement run", () => {
    const store = createStore();
    store.select(A);
    const first = store.beginRun(A);
    store.setCurrentResponseId(A, first.token, "old-response");
    expect(store.finishRun(A, first.token)).toBe(true);
    let synchronousAbortCancellation: ReturnType<typeof store.cancelRun> | undefined;
    first.signal.addEventListener("abort", () => {
      synchronousAbortCancellation = store.cancelRun(A, first.token);
    });

    const replacement = store.beginRun(A);
    store.setCurrentResponseId(A, replacement.token, "new-response");
    store.setAssistantStreaming(A, replacement.token, true);
    first.controller.abort();

    expect(first.signal.aborted).toBe(true);
    expect(synchronousAbortCancellation).toBeNull();
    expect(replacement.signal.aborted).toBe(false);
    expect(store.isRunCurrent(A, replacement.token)).toBe(true);
    expect(store.get(A)).toMatchObject({
      isGenerating: true,
      assistantStreaming: true,
      currentResponseId: "new-response",
      runToken: replacement.token
    });
  });

  test("refuses to replace an active same-chat run", () => {
    const store = createStore();
    store.select(A);
    const activeRun = store.beginRun(A);
    store.setCurrentResponseId(A, activeRun.token, "active-response");

    expect(() => store.beginRun(A)).toThrow("already has an active run");
    expect(activeRun.signal.aborted).toBe(false);
    expect(store.isRunCurrent(A, activeRun.token)).toBe(true);
    expect(store.get(A)).toMatchObject({
      isGenerating: true,
      currentResponseId: "active-response",
      runToken: activeRun.token
    });
  });

  test("beginRun always returns ownership before fallible resource eviction", () => {
    const store = createStore({
      maxInactiveCompletedEntries: 0,
      disposeEntry: () => {
        throw new Error("resource disposal failed");
      }
    });
    store.ensure(B);

    const run = store.beginRun(A);

    expect(store.isRunCurrent(A, run.token)).toBe(true);
    expect(run.signal.aborted).toBe(false);
    expect(store.get(A)).toMatchObject({
      isGenerating: true,
      runToken: run.token
    });
  });

  test("updateForRun cannot expose an idle snapshot while its controller is active", () => {
    const store = createStore();
    store.select(A);
    const run = store.beginRun(A);
    const unsafeActiveSnapshot = store.get(A)!;
    const unsafeUpdater = (() => ({
      ...unsafeActiveSnapshot,
      isGenerating: false
    })) as unknown as ChatRuntimeRunUpdater<Conversation, Message, Composer>;

    expect(store.updateForRun(A, run.token, unsafeUpdater)).toBe(true);

    expect(store.isRunCurrent(A, run.token)).toBe(true);
    expect(store.get(A)).toMatchObject({
      isGenerating: true,
      runToken: run.token
    });
    expect(() => store.beginRun(A)).toThrow("already has an active run");
  });

  test("tokenless updates cannot overwrite run-owned replacement fields", () => {
    const store = createStore();
    store.select(A);
    const staleSnapshot = store.get(A)!;
    const activeRun = store.beginRun(A);
    store.setCurrentResponseId(A, activeRun.token, "active-response");
    store.setAssistantStreaming(A, activeRun.token, true);

    // A stale callback may still return a whole pre-run snapshot at runtime.
    // Generic updates must accept its safe fields while retaining run ownership.
    store.update(A, () => ({
      ...staleSnapshot,
      composer: { input: "safe composer update", attachmentIds: [] }
    }));

    expect(store.get(A)).toMatchObject({
      composer: { input: "safe composer update", attachmentIds: [] },
      isGenerating: true,
      currentResponseId: "active-response",
      assistantStreaming: true,
      runToken: activeRun.token
    });
  });

  test("publishes stable global and keyed snapshot revisions", () => {
    const store = createStore();
    store.ensure(A);
    store.ensure(B);
    const initialGlobalRevision = store.getSubscriberRevision();
    const initialARevision = store.get(A)?.revision;
    let globalNotifications = 0;
    let aNotifications = 0;
    let bNotifications = 0;
    const unsubscribeGlobal = store.subscribe(() => {
      globalNotifications += 1;
    });
    const unsubscribeA = store.subscribeKey(A, () => {
      aNotifications += 1;
    });
    const unsubscribeB = store.subscribeKey(B, () => {
      bNotifications += 1;
    });

    store.update(B, (snapshot) => ({ ...snapshot, historyLoaded: true }));
    expect(aNotifications).toBe(0);
    expect(bNotifications).toBe(1);
    store.update(A, (snapshot) => ({ ...snapshot, historyLoaded: true }));

    expect(store.getSubscriberRevision()).toBe(initialGlobalRevision + 2);
    expect(store.get(A)?.revision).toBe((initialARevision ?? 0) + 1);
    expect(globalNotifications).toBe(2);
    expect(aNotifications).toBe(1);
    expect(bNotifications).toBe(1);

    unsubscribeGlobal();
    unsubscribeA();
    unsubscribeB();
    store.update(A, (snapshot) => ({ ...snapshot, error: "after unsubscribe" }));
    expect(globalNotifications).toBe(2);
    expect(aNotifications).toBe(1);
  });

  test("publishes active-run keys only when run membership changes", () => {
    const store = createStore();
    const snapshots: string[][] = [];
    const initialSnapshot = store.getActiveRunKeys();
    store.subscribeActiveRuns(() => {
      snapshots.push([...store.getActiveRunKeys()]);
    });

    const runA = store.beginRun(A);
    const afterA = store.getActiveRunKeys();
    expect(afterA).toEqual([A]);
    expect(Object.isFrozen(afterA)).toBe(true);

    store.updateForRun(A, runA.token, (snapshot) => ({
      ...snapshot,
      messages: [streamingMessage("a-assistant", "delta")]
    }));
    expect(store.getActiveRunKeys()).toBe(afterA);
    expect(snapshots).toEqual([[A]]);

    const runB = store.beginRun(B);
    expect(new Set(store.getActiveRunKeys())).toEqual(new Set([A, B]));
    expect(store.finishRun(A, runA.token)).toBe(true);
    expect(store.getActiveRunKeys()).toEqual([B]);
    expect(store.cancelRun(B, runB.token)?.key).toBe(B);
    expect(store.getActiveRunKeys()).toBe(initialSnapshot);

    const draft = createChatDraftKey("active-rekey");
    const conversation = createConversationChatKey("active-rekeyed-conversation");
    const draftRun = store.beginRun(draft);
    expect(store.getActiveRunKeys()).toEqual([draft]);
    expect(store.rekey(draft, conversation, draftRun.token)).toBe(conversation);
    expect(store.getActiveRunKeys()).toEqual([conversation]);

    store.dispose();
    expect(store.getActiveRunKeys()).toBe(initialSnapshot);
    expect(snapshots).toEqual([[A], [A, B], [B], [], [draft], [conversation], []]);
  });

  test("uses identity-safe visible-chat ownership to classify successful completions", () => {
    const store = createStore();
    const owner = {};

    const staleLease = store.claimVisibleChat(owner, A);
    const replacementLease = store.claimVisibleChat(owner, B);
    expect(Object.isFrozen(staleLease)).toBe(true);
    expect(Object.isFrozen(replacementLease)).toBe(true);
    expect(replacementLease.sequence).toBeGreaterThan(staleLease.sequence);
    store.releaseVisibleChat(staleLease);
    expect(store.isChatVisible(A)).toBe(false);
    expect(store.isChatVisible(B)).toBe(true);

    const visibleRun = store.beginRun(B, { groupId: "project-visible" });
    expect(store.completeRun(B, visibleRun.token)).toBe(true);
    expect(store.getCompletedUnreadKeys()).toEqual([]);

    const offscreenRun = store.beginRun(A, { groupId: "project-offscreen" });
    expect(store.completeRun(A, offscreenRun.token)).toBe(true);
    expect(store.getCompletedUnreadKeys()).toEqual([A]);
    expect(store.getCompletedUnreadGroupId(A)).toBe("project-offscreen");

    store.releaseVisibleChat(replacementLease);
    expect(store.isChatVisible(B)).toBe(false);
    const formerlyVisibleRun = store.beginRun(B, { groupId: "project-visible" });
    expect(store.completeRun(B, formerlyVisibleRun.token)).toBe(true);
    expect(new Set(store.getCompletedUnreadKeys())).toEqual(new Set([A, B]));
  });

  test("clears completed-unread state on visibility, a new run, and an explicit read", () => {
    const store = createStore();
    const owner = {};

    const first = store.beginRun(A, { groupId: "project-a" });
    expect(store.completeRun(A, first.token)).toBe(true);
    expect(store.getCompletedUnreadKeys()).toEqual([A]);

    store.select(A);
    expect(store.getCompletedUnreadKeys()).toEqual([A]);
    const visibleLease = store.claimVisibleChat(owner, A);
    expect(store.getCompletedUnreadKeys()).toEqual([]);
    store.releaseVisibleChat(visibleLease);

    store.clearSelection();
    const second = store.beginRun(A, { groupId: "project-a" });
    expect(store.completeRun(A, second.token)).toBe(true);
    expect(store.getCompletedUnreadKeys()).toEqual([A]);

    const replacement = store.beginRun(A, { groupId: "project-a" });
    expect(store.getCompletedUnreadKeys()).toEqual([]);
    expect(store.finishRun(A, replacement.token)).toBe(true);

    const third = store.beginRun(A, { groupId: "project-a" });
    expect(store.completeRun(A, third.token)).toBe(true);
    expect(store.markCompletionRead(A)).toBe(true);
    expect(store.markCompletionRead(A)).toBe(false);
    expect(store.getCompletedUnreadKeys()).toEqual([]);
  });

  test("does not mark failures, cancellations, or stale completions unread", () => {
    const store = createStore();

    const failed = store.beginRun(A, { groupId: "project-a" });
    expect(store.finishRun(A, failed.token)).toBe(true);
    expect(store.getCompletedUnreadKeys()).toEqual([]);

    const cancelled = store.beginRun(B, { groupId: "project-b" });
    expect(store.cancelRun(B, cancelled.token)?.key).toBe(B);
    expect(store.completeRun(B, cancelled.token)).toBe(false);
    expect(store.getCompletedUnreadKeys()).toEqual([]);

    const stale = store.beginRun(A, { groupId: "project-old" });
    expect(store.finishRun(A, stale.token)).toBe(true);
    const current = store.beginRun(A, { groupId: "project-current" });

    expect(store.completeRun(A, stale.token)).toBe(false);
    expect(store.getCompletedUnreadKeys()).toEqual([]);
    expect(store.isRunCurrent(A, current.token)).toBe(true);
    expect(store.finishRun(A, current.token)).toBe(true);
  });

  test("keeps unread completion canonical through draft rekey and visible aliases", () => {
    const store = createStore();
    const draft = createChatDraftKey("completion-alias");
    const conversation = createConversationChatKey("completion-canonical");
    const run = store.beginRun(draft, { groupId: "project-a" });

    expect(store.rekey(draft, conversation, run.token)).toBe(conversation);
    expect(store.completeRun(draft, run.token)).toBe(true);
    expect(store.getCompletedUnreadKeys()).toEqual([conversation]);
    expect(store.getCompletedUnreadGroupId(draft)).toBe("project-a");

    expect(store.markCompletionRead(draft)).toBe(true);
    expect(store.getCompletedUnreadKeys()).toEqual([]);

    const owner = {};
    const visibleDraft = createChatDraftKey("visible-completion-alias");
    const visibleConversation = createConversationChatKey("visible-completion-canonical");
    const visibleRun = store.beginRun(visibleDraft, { groupId: "project-b" });
    store.claimVisibleChat(owner, visibleDraft);

    expect(store.rekey(visibleDraft, visibleConversation, visibleRun.token)).toBe(
      visibleConversation
    );
    expect(store.completeRun(visibleDraft, visibleRun.token)).toBe(true);
    expect(store.getCompletedUnreadKeys()).toEqual([]);
  });

  test("retains unread metadata across LRU eviction and clears unread-only deletion", () => {
    const store = createStore({ maxInactiveCompletedEntries: 0 });
    const run = store.beginRun(A, { groupId: "project-a" });

    expect(store.completeRun(A, run.token)).toBe(true);
    expect(store.get(A)).toBeUndefined();
    expect(store.getCompletedUnreadKeys()).toEqual([A]);
    expect(store.getCompletedUnreadGroupId(A)).toBe("project-a");

    expect(store.delete(A)).toBe(true);
    expect(store.getCompletedUnreadKeys()).toEqual([]);
    expect(store.getCompletedUnreadGroupId(A)).toBeUndefined();
    expect(store.delete(A)).toBe(false);
  });

  test("publishes stable, targeted activity notifications for group changes", () => {
    const store = createStore();
    let activeNotifications = 0;
    let unreadNotifications = 0;
    store.subscribeActiveRuns(() => {
      activeNotifications += 1;
    });
    store.subscribeCompletedUnread(() => {
      unreadNotifications += 1;
    });

    const runA = store.beginRun(A, { groupId: "project-one" });
    const activeAfterBegin = store.getActiveRunKeys();
    expect(activeNotifications).toBe(1);
    expect(unreadNotifications).toBe(0);
    expect(Object.isFrozen(activeAfterBegin)).toBe(true);

    store.updateForRun(A, runA.token, (snapshot) => ({
      ...snapshot,
      messages: [streamingMessage("a-assistant", "delta")]
    }));
    expect(store.getActiveRunKeys()).toBe(activeAfterBegin);
    expect(activeNotifications).toBe(1);
    expect(unreadNotifications).toBe(0);

    expect(store.updateActivityGroup(A, "project-two")).toBe(true);
    const activeAfterGroupChange = store.getActiveRunKeys();
    expect(activeAfterGroupChange).toEqual([A]);
    expect(activeAfterGroupChange).not.toBe(activeAfterBegin);
    expect(store.getActiveRunGroupId(A)).toBe("project-two");
    expect(activeNotifications).toBe(2);
    expect(unreadNotifications).toBe(0);

    expect(store.updateActivityGroup(A, "project-two")).toBe(false);
    expect(store.getActiveRunKeys()).toBe(activeAfterGroupChange);
    expect(activeNotifications).toBe(2);

    expect(store.completeRun(A, runA.token)).toBe(true);
    const unreadAfterCompletion = store.getCompletedUnreadKeys();
    expect(unreadAfterCompletion).toEqual([A]);
    expect(Object.isFrozen(unreadAfterCompletion)).toBe(true);
    expect(activeNotifications).toBe(3);
    expect(unreadNotifications).toBe(1);

    expect(store.updateActivityGroup(A, "project-three")).toBe(true);
    expect(store.getCompletedUnreadKeys()).toEqual([A]);
    expect(store.getCompletedUnreadKeys()).not.toBe(unreadAfterCompletion);
    expect(store.getCompletedUnreadGroupId(A)).toBe("project-three");
    expect(activeNotifications).toBe(3);
    expect(unreadNotifications).toBe(2);

    const runB = store.beginRun(B, { groupId: "project-three" });
    expect(store.reassignActivityGroup("project-three", "project-four")).toBe(true);
    expect(store.getActiveRunGroupId(B)).toBe("project-four");
    expect(store.getCompletedUnreadGroupId(A)).toBe("project-four");
    expect(activeNotifications).toBe(5);
    expect(unreadNotifications).toBe(3);

    expect(store.finishRun(B, runB.token)).toBe(true);
  });

  test("deletes active and unread members of an activity group", () => {
    const store = createStore();
    const idle = createConversationChatKey("idle-project-member");
    const activeRun = store.beginRun(A, { groupId: "project-delete" });
    const unreadRun = store.beginRun(B, { groupId: "project-delete" });
    expect(store.completeRun(B, unreadRun.token)).toBe(true);
    store.ensure(idle, { historyLoaded: true });
    expect(store.updateActivityGroup(idle, "project-delete")).toBe(true);
    expect(store.getActivityGroupId(idle)).toBe("project-delete");

    const deleted = store.deleteActivityGroup("project-delete");

    expect(new Set(deleted)).toEqual(new Set([A, B, idle]));
    expect(Object.isFrozen(deleted)).toBe(true);
    expect(activeRun.signal.aborted).toBe(true);
    expect(store.get(A)).toBeUndefined();
    expect(store.get(B)).toBeUndefined();
    expect(store.get(idle)).toBeUndefined();
    expect(store.getActiveRunKeys()).toEqual([]);
    expect(store.getCompletedUnreadKeys()).toEqual([]);
    expect(store.deleteActivityGroup("project-delete")).toEqual([]);
  });

  test("clears every runtime and resource while keeping the store reusable", () => {
    const disposed: Array<{ key: string; reason: string }> = [];
    const store = createStore({
      disposeEntry: (snapshot, reason) => disposed.push({ key: snapshot.key, reason })
    });
    const draft = createChatDraftKey("clear-all-draft");
    store.select(draft, { composer: { input: "unsent", attachmentIds: ["blob"] } });
    store.rememberDraftKey(null, draft);
    const active = store.beginRun(A, { groupId: "project-clear" });
    const unread = store.beginRun(B, { groupId: "project-clear" });
    expect(store.completeRun(B, unread.token)).toBe(true);
    store.claimVisibleChat({}, A);

    const cleared = store.clearAll();

    expect(new Set(cleared)).toEqual(new Set([draft, A, B]));
    expect(Object.isFrozen(cleared)).toBe(true);
    expect(active.signal.aborted).toBe(true);
    expect(store.getActive()).toBeUndefined();
    expect(store.getActiveRunKeys()).toEqual([]);
    expect(store.getCompletedUnreadKeys()).toEqual([]);
    expect(store.getRememberedDraftKey(null)).toBeNull();
    expect(new Set(disposed)).toEqual(
      new Set([
        { key: draft, reason: "deleted" },
        { key: A, reason: "deleted" },
        { key: B, reason: "deleted" }
      ])
    );

    expect(store.select(createChatDraftKey("after-clear"))).toBeDefined();
  });

  test("dispose resets unread and visible ownership state and notifies subscribers", () => {
    const store = createStore();
    const emptyActiveSnapshot = store.getActiveRunKeys();
    const emptyUnreadSnapshot = store.getCompletedUnreadKeys();
    const owner = {};
    const completed = store.beginRun(A, { groupId: "project-a" });
    expect(store.completeRun(A, completed.token)).toBe(true);
    const active = store.beginRun(B, { groupId: "project-b" });
    const visibleLease = store.claimVisibleChat(owner, B);
    let activeNotifications = 0;
    let unreadNotifications = 0;
    store.subscribeActiveRuns(() => {
      activeNotifications += 1;
    });
    store.subscribeCompletedUnread(() => {
      unreadNotifications += 1;
    });

    store.dispose();

    expect(active.signal.aborted).toBe(true);
    expect(store.getActiveRunKeys()).toBe(emptyActiveSnapshot);
    expect(store.getCompletedUnreadKeys()).toBe(emptyUnreadSnapshot);
    expect(store.getCompletedUnreadGroupId(A)).toBeUndefined();
    expect(activeNotifications).toBe(1);
    expect(unreadNotifications).toBe(1);
    expect(() => store.claimVisibleChat(owner, A)).toThrow("has been disposed");
    expect(() => store.releaseVisibleChat(visibleLease)).not.toThrow();
    expect(store.isChatVisible(B)).toBe(false);
    expect(() => store.markCompletionRead(A)).toThrow("has been disposed");
    expect(() => store.updateActivityGroup(A, null)).toThrow("has been disposed");
    expect(() => store.reassignActivityGroup("project-a", null)).toThrow("has been disposed");
    expect(() => store.deleteActivityGroup("project-a")).toThrow("has been disposed");
    expect(() => store.subscribeCompletedUnread(() => {})).toThrow("has been disposed");
  });

  test("evicts only least-recent inactive completed entries and disposes their resources", () => {
    const disposed: Array<{ key: string; reason: string }> = [];
    const store = createStore({
      maxInactiveCompletedEntries: 1,
      disposeEntry: (snapshot, reason) => disposed.push({ key: snapshot.key, reason })
    });
    const C = createConversationChatKey("c");
    const D = createConversationChatKey("d");
    store.select(A);
    store.ensure(B);
    store.ensure(C);

    expect(store.get(B)).toBeUndefined();
    expect(store.get(C)).toBeDefined();
    expect(disposed).toEqual([{ key: B, reason: "evicted" }]);

    const runD = store.beginRun(D);
    expect(store.get(C)).toBeDefined();
    expect(store.get(D)?.isGenerating).toBe(true);
    store.finishRun(D, runD.token);

    expect(store.get(C)).toBeUndefined();
    expect(store.get(D)).toBeDefined();
    expect(disposed).toContainEqual({ key: C, reason: "evicted" });

    store.dispose();
    expect(disposed).toContainEqual({ key: A, reason: "disposed" });
    expect(disposed).toContainEqual({ key: D, reason: "disposed" });
  });

  test("retains an evicted draft conversation alias for Back but clears it on delete", () => {
    const store = createStore({ maxInactiveCompletedEntries: 0 });
    const draft = createChatDraftKey("offscreen-history");
    const conversation = createConversationChatKey("offscreen-created");

    store.select(draft);
    const run = store.beginRun(draft);
    expect(store.rekey(draft, conversation, run.token)).toBe(conversation);
    store.finishRun(conversation, run.token);

    store.select(A);
    expect(store.get(conversation)).toBeUndefined();
    expect(store.resolveKey(draft)).toBe(conversation);

    const restored = store.select(draft);
    expect(restored.key).toBe(conversation);
    expect(store.getActiveKey()).toBe(conversation);

    store.select(A);
    expect(store.get(conversation)).toBeUndefined();
    expect(store.delete(conversation)).toBe(true);
    expect(store.resolveKey(draft)).toBe(draft);
    expect(store.get(draft)).toBeUndefined();
  });

  test("dispose is terminal, clears subscriptions, and is safe when callbacks reenter", () => {
    const disposedKeys: string[] = [];
    const store = createStore({
      disposeEntry: (snapshot) => {
        disposedKeys.push(snapshot.key);
        store.dispose();
        expect(() => store.ensure(createConversationChatKey("late-callback"))).toThrow(
          "has been disposed"
        );
      }
    });
    store.select(A);
    store.ensure(B);
    const run = store.beginRun(A);
    let notifications = 0;
    const unsubscribe = store.subscribe(() => {
      notifications += 1;
      store.dispose();
      expect(() => store.clearSelection()).toThrow("has been disposed");
    });

    store.dispose();

    expect(run.signal.aborted).toBe(true);
    expect(disposedKeys).toEqual([A, B]);
    expect(notifications).toBe(1);
    expect(store.getActiveKey()).toBeNull();
    expect(store.get(A)).toBeUndefined();
    expect(() => store.select(A)).toThrow("has been disposed");
    expect(store.updateForRun(A, run.token, (snapshot) => snapshot)).toBe(false);
    expect(store.finishRun(A, run.token)).toBe(false);
    expect(store.completeRun(A, run.token)).toBe(false);
    expect(store.cancelRun(A, run.token)).toBeNull();
    expect(store.rekey(A, createConversationChatKey("late-created"), run.token)).toBeNull();
    expect(() => store.beginRun(A)).toThrow("has been disposed");
    expect(() => store.subscribe(() => {})).toThrow("has been disposed");

    unsubscribe();
    store.dispose();
    expect(disposedKeys).toEqual([A, B]);
    expect(notifications).toBe(1);
  });

  test("dispose completes every abort, resource callback, and listener when some throw", () => {
    const disposedKeys: string[] = [];
    const store = createStore({
      disposeEntry: (snapshot) => {
        disposedKeys.push(snapshot.key);
        if (snapshot.key === A) throw new Error("resource disposal failed");
      }
    });
    store.select(A);
    const runA = store.beginRun(A);
    store.select(B);
    const runB = store.beginRun(B);
    let laterListenerCalls = 0;
    store.subscribe(() => {
      throw new Error("listener failed");
    });
    store.subscribe(() => {
      laterListenerCalls += 1;
    });

    expect(() => store.dispose()).toThrow("resource disposal failed");

    expect(runA.signal.aborted).toBe(true);
    expect(runB.signal.aborted).toBe(true);
    expect(disposedKeys).toEqual([A, B]);
    expect(laterListenerCalls).toBe(1);
    expect(store.get(A)).toBeUndefined();
    expect(store.get(B)).toBeUndefined();
    expect(() => store.ensure(A)).toThrow("has been disposed");
  });

  test("subscriber exceptions cannot strand beginRun or hide cancellation metadata", () => {
    const store = createStore();
    store.select(A);
    let laterListenerCalls = 0;
    store.subscribe(() => {
      throw new Error("broken subscriber");
    });
    store.subscribe(() => {
      laterListenerCalls += 1;
    });

    const run = store.beginRun(A);
    expect(store.isRunCurrent(A, run.token)).toBe(true);
    expect(run.signal.aborted).toBe(false);
    expect(store.setCurrentResponseId(A, run.token, "response-a")).toBe(true);

    const cancelled = store.cancelRun(A, run.token);

    expect(cancelled).toEqual({
      key: A,
      token: run.token,
      responseId: "response-a",
      controller: run.controller
    });
    expect(run.signal.aborted).toBe(true);
    expect(store.get(A)?.runToken).toBeNull();
    expect(laterListenerCalls).toBe(3);
  });
});
