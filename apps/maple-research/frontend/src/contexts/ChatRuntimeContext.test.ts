import { describe, expect, spyOn, test } from "bun:test";
import {
  composerHasRetainedDraft,
  createChatComposerState,
  createChatRuntimeStore
} from "./ChatRuntimeContext";
import { cancelActiveChatRuntimeRuns } from "@/services/chatRuntimeCancellation";
import {
  draftScopeForRuntimeSelection,
  moveRememberedChatDraftToScope,
  rememberChatDraftInScope,
  resumeOrCreateChatDraftKey,
  rootChatDraftKeyAfterProjectDeletion
} from "../services/chatDraftSelection";
import { createChatDraftKey, createConversationChatKey } from "../services/chatRuntimeStore";
import {
  mergeChatComposerDraftsForRekey,
  type ChatQueuedMessage
} from "../services/chatComposerQueue";

type Conversation = { id: string };
type Message = { id: string };

function queuedMessage(id: string, overrides: Partial<ChatQueuedMessage> = {}): ChatQueuedMessage {
  return {
    queueId: `queue-${id}`,
    messageId: `message-${id}`,
    text: id,
    draftImages: [],
    imageUrls: new Map(),
    documentText: "",
    documentName: "",
    draftProjectId: null,
    model: "maple-model",
    webSearchEnabled: false,
    createdMs: 0,
    ...overrides
  };
}

describe("ChatRuntimeContext eviction policy", () => {
  test("account teardown synchronously cancels every active queue runner", () => {
    const store = createChatRuntimeStore<Conversation, Message>();
    const firstKey = createConversationChatKey("first-running-conversation");
    const secondKey = createConversationChatKey("second-running-conversation");
    store.ensure(firstKey);
    store.ensure(secondKey);
    const first = store.beginRun(firstKey);
    const second = store.beginRun(secondKey);

    cancelActiveChatRuntimeRuns(store);

    expect(first.signal.aborted).toBe(true);
    expect(second.signal.aborted).toBe(true);
    expect(store.getActiveRunKeys()).toEqual([]);
    expect(store.get(firstKey)?.composer).toBeDefined();
    expect(store.get(secondKey)?.composer).toBeDefined();
  });

  test("retains an idle runtime whose only unsent material is its queue", () => {
    const store = createChatRuntimeStore<Conversation, Message>(0);
    const queuedKey = createConversationChatKey("queued-conversation");
    const nextKey = createConversationChatKey("next-conversation");
    const composer = createChatComposerState();
    composer.queue.items = [queuedMessage("retained")];

    store.select(queuedKey, { composer });
    store.select(nextKey, { conversation: { id: "next-conversation" } });

    expect(store.get(queuedKey)?.composer.queue.items).toEqual([
      expect.objectContaining({ queueId: "queue-retained" })
    ]);
    expect(composerHasRetainedDraft(queuedKey, composer)).toBe(true);
  });

  test("retains and restores an idle draft with unsent composer content across selection", () => {
    const store = createChatRuntimeStore<Conversation, Message>(0);
    const draftKey = createChatDraftKey("retained-unsent-composer");
    const conversationKey = createConversationChatKey("visited-conversation");
    const composer = createChatComposerState("project-a");
    composer.input = "keep this unfinished message";
    composer.documentText = "document payload";
    composer.documentName = "notes.md";

    store.select(draftKey, { composer });
    store.select(conversationKey, { conversation: { id: "visited-conversation" } });

    expect(store.get(draftKey)?.composer).toMatchObject({
      input: "keep this unfinished message",
      draftProjectId: "project-a",
      documentText: "document payload",
      documentName: "notes.md"
    });

    store.select(draftKey);
    expect(store.getActiveKey()).toBe(draftKey);
    expect(store.getActive()?.composer.input).toBe("keep this unfinished message");
  });

  test("resumes the remembered offscreen New Chat draft with its exact composer resources", () => {
    const revokeObjectURL = spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    const store = createChatRuntimeStore<Conversation, Message>();
    const draftKey = createChatDraftKey("remembered-global");
    const conversationKey = createConversationChatKey("visited-conversation");
    const image = new File(["image"], "draft.png", { type: "image/png" });
    const composer = createChatComposerState();
    composer.input = "unfinished thought";
    composer.draftImages = [image];
    composer.imageUrls.set(image, "blob:remembered-global");
    composer.documentText = "draft document";
    composer.documentName = "draft.md";

    store.select(draftKey, { composer });
    store.rememberDraftKey(null, draftKey);
    store.claimVisibleChat({}, draftKey);
    store.select(conversationKey, { conversation: { id: "visited-conversation" } });
    store.claimVisibleChat({}, conversationKey);

    try {
      const resumedKey = resumeOrCreateChatDraftKey(store, null, () => {
        throw new Error("should not create a replacement draft");
      });

      store.select(resumedKey, { composer: createChatComposerState() });
      store.claimVisibleChat({}, resumedKey);

      expect(resumedKey).toBe(draftKey);
      expect(store.get(resumedKey)?.composer).toBe(composer);
      expect(store.get(resumedKey)?.composer.draftImages[0]).toBe(image);
      expect(store.get(resumedKey)?.composer.imageUrls.get(image)).toBe("blob:remembered-global");
      expect(store.get(resumedKey)?.composer.documentText).toBe("draft document");

      const replacementKey = createChatDraftKey("replacement-global");
      expect(resumeOrCreateChatDraftKey(store, null, () => replacementKey)).toBe(replacementKey);
      expect(store.get(draftKey)?.composer).toBe(composer);
      expect(revokeObjectURL).not.toHaveBeenCalled();
    } finally {
      store.dispose();
      revokeObjectURL.mockRestore();
    }
  });

  test("keeps remembered New Chat drafts isolated by project scope", () => {
    const store = createChatRuntimeStore<Conversation, Message>();
    const globalKey = createChatDraftKey("global");
    const projectAKey = createChatDraftKey("project-a");
    const projectBKey = createChatDraftKey("project-b");

    const globalComposer = createChatComposerState();
    globalComposer.input = "global draft";
    const projectAComposer = createChatComposerState("project-a");
    projectAComposer.input = "project A draft";
    const projectBComposer = createChatComposerState("project-b");
    projectBComposer.input = "project B draft";

    store.ensure(globalKey, { composer: globalComposer });
    store.ensure(projectAKey, { composer: projectAComposer });
    store.ensure(projectBKey, { composer: projectBComposer });
    store.rememberDraftKey(null, globalKey);
    store.rememberDraftKey("project-a", projectAKey);
    store.rememberDraftKey("project-b", projectBKey);

    expect(resumeOrCreateChatDraftKey(store, null)).toBe(globalKey);
    expect(resumeOrCreateChatDraftKey(store, "project-a")).toBe(projectAKey);
    expect(resumeOrCreateChatDraftKey(store, "project-b")).toBe(projectBKey);
  });

  test("preserves an existing root scope instead of falling back to the current project", () => {
    const store = createChatRuntimeStore<Conversation, Message>();
    const rootKey = createChatDraftKey("existing-root-scope");
    const missingKey = createChatDraftKey("missing-scope");
    const composer = createChatComposerState(null);
    composer.input = "root draft";
    store.ensure(rootKey, { composer });

    expect(draftScopeForRuntimeSelection(store, rootKey, "project-a")).toBeNull();
    expect(draftScopeForRuntimeSelection(store, missingKey, "project-a")).toBe("project-a");
  });

  test("mount registration gives a restored project draft deletion ownership", () => {
    const store = createChatRuntimeStore<Conversation, Message>();
    const projectKey = createChatDraftKey("mounted-project-draft");
    const composer = createChatComposerState("project-a");
    composer.input = "mounted project draft";
    store.ensure(projectKey, { composer });

    expect(rememberChatDraftInScope(store, projectKey, "project-a")).toBe(true);
    expect(store.getRememberedDraftKey("project-a")).toBe(projectKey);
    expect(store.deleteActivityGroup("project-a")).toEqual([projectKey]);
    expect(store.get(projectKey)).toBeUndefined();
  });

  test("creates a fresh draft instead of reusing the currently visible or running draft", () => {
    const store = createChatRuntimeStore<Conversation, Message>();
    const visibleKey = createChatDraftKey("visible");
    const nextKey = createChatDraftKey("next");
    const runningKey = createChatDraftKey("running");
    const afterRunningKey = createChatDraftKey("after-running");
    const visibleComposer = createChatComposerState();
    visibleComposer.input = "keep the old visible draft";

    store.select(visibleKey, { composer: visibleComposer });
    store.rememberDraftKey(null, visibleKey);
    store.claimVisibleChat({}, visibleKey);

    expect(resumeOrCreateChatDraftKey(store, null, () => nextKey)).toBe(nextKey);
    expect(store.getRememberedDraftKey(null)).toBe(nextKey);
    expect(store.get(nextKey)?.composer.draftProjectId).toBeNull();

    store.select(runningKey);
    store.rememberDraftKey(null, runningKey);
    store.beginRun(runningKey);

    expect(resumeOrCreateChatDraftKey(store, null, () => afterRunningKey)).toBe(afterRunningKey);
    expect(store.getRememberedDraftKey(null)).toBe(afterRunningKey);
    expect(store.get(afterRunningKey)).toBeDefined();
  });

  test("a late production-style draft adoption cannot clear its materialized replacement", () => {
    const store = createChatRuntimeStore<Conversation, Message>();
    const sourceKey = createChatDraftKey("source");
    const replacementKey = createChatDraftKey("replacement");
    const conversationKey = createConversationChatKey("created-conversation");
    const image = new File(["replacement"], "replacement.png", { type: "image/png" });
    const replacementComposer = createChatComposerState();
    replacementComposer.input = "a second prompt";
    replacementComposer.draftImages = [image];
    replacementComposer.imageUrls.set(image, "blob:replacement");
    replacementComposer.documentText = "replacement document";

    store.select(sourceKey);
    store.rememberDraftKey(null, sourceKey);
    store.claimVisibleChat({}, sourceKey);
    const run = store.beginRun(sourceKey);
    expect(resumeOrCreateChatDraftKey(store, null, () => replacementKey)).toBe(replacementKey);
    store.select(replacementKey);
    store.update(replacementKey, (snapshot) => ({ ...snapshot, composer: replacementComposer }));
    store.claimVisibleChat({}, replacementKey);
    store.ensure(conversationKey, {
      conversation: { id: "created-conversation" },
      historyLoaded: true
    });

    expect(
      store.rekeyRunAdoptingIdleDestination(
        sourceKey,
        conversationKey,
        run.token,
        (source, destination) => ({
          ...source,
          conversation: destination.conversation,
          historyLoaded: destination.historyLoaded
        })
      )
    ).toMatchObject({ status: "migrated", key: conversationKey });
    expect(store.getRememberedDraftKey(null)).toBe(replacementKey);
    store.select(conversationKey);
    store.claimVisibleChat({}, conversationKey);
    expect(resumeOrCreateChatDraftKey(store, null)).toBe(replacementKey);
    expect(store.get(replacementKey)?.composer).toBe(replacementComposer);
    expect(store.get(replacementKey)?.composer.draftImages[0]).toBe(image);
    expect(store.isRunCurrent(conversationKey, run.token)).toBe(true);
    expect(run.signal.aborted).toBe(false);
  });

  test("a normal draft rekey clears only its own remembered scope", () => {
    const store = createChatRuntimeStore<Conversation, Message>();
    const sourceKey = createChatDraftKey("normally-rekeyed-source");
    const otherProjectKey = createChatDraftKey("unrelated-project-draft");
    const conversationKey = createConversationChatKey("normally-created-conversation");
    const otherComposer = createChatComposerState("project-b");
    otherComposer.input = "unrelated project draft";

    store.select(sourceKey);
    store.rememberDraftKey(null, sourceKey);
    store.ensure(otherProjectKey, { composer: otherComposer });
    store.rememberDraftKey("project-b", otherProjectKey);
    const run = store.beginRun(sourceKey);

    expect(store.rekey(sourceKey, conversationKey, run.token)).toBe(conversationKey);
    expect(store.getRememberedDraftKey(null)).toBeNull();
    expect(store.getRememberedDraftKey("project-b")).toBe(otherProjectKey);
  });

  test("moving a remembered draft updates deletion ownership without losing resources", () => {
    const store = createChatRuntimeStore<Conversation, Message>();
    const draftKey = createChatDraftKey("moved-project-draft");
    const image = new File(["moved"], "moved.png", { type: "image/png" });
    const composer = createChatComposerState("project-a");
    composer.input = "keep this while moving projects";
    composer.draftImages = [image];
    composer.imageUrls.set(image, "blob:moved-project-draft");

    store.select(draftKey, { composer });
    store.rememberDraftKey("project-a", draftKey);
    store.updateActivityGroup(draftKey, "project-a");

    expect(moveRememberedChatDraftToScope(store, draftKey, "project-b")).toBe(true);
    expect(store.getRememberedDraftKey("project-a")).toBeNull();
    expect(store.getRememberedDraftKey("project-b")).toBe(draftKey);
    expect(store.getActivityGroupId(draftKey)).toBe("project-b");
    expect(store.deleteActivityGroup("project-a")).toEqual([]);
    expect(resumeOrCreateChatDraftKey(store, "project-b")).toBe(draftKey);
    expect(store.get(draftKey)?.composer.input).toBe("keep this while moving projects");
    expect(store.get(draftKey)?.composer.draftImages[0]).toBe(image);
    expect(store.get(draftKey)?.composer.imageUrls).toBe(composer.imageUrls);
    expect(store.get(draftKey)?.composer.imageUrls.get(image)).toBe("blob:moved-project-draft");

    expect(moveRememberedChatDraftToScope(store, draftKey, null)).toBe(true);
    expect(store.getRememberedDraftKey("project-b")).toBeNull();
    expect(store.getRememberedDraftKey(null)).toBe(draftKey);
    expect(store.deleteActivityGroup("project-b")).toEqual([]);
    expect(resumeOrCreateChatDraftKey(store, null)).toBe(draftKey);
  });

  test("deletion clears the remembered draft for that runtime or project", () => {
    const store = createChatRuntimeStore<Conversation, Message>();
    const globalKey = createChatDraftKey("deleted-global");
    const projectKey = createChatDraftKey("deleted-project");
    const nextGlobalKey = createChatDraftKey("next-global");
    const nextProjectKey = createChatDraftKey("next-project");

    store.ensure(globalKey);
    store.rememberDraftKey(null, globalKey);
    expect(store.delete(globalKey)).toBe(true);
    expect(resumeOrCreateChatDraftKey(store, null, () => nextGlobalKey)).toBe(nextGlobalKey);

    store.ensure(projectKey, { composer: createChatComposerState("project-a") });
    store.updateActivityGroup(projectKey, "project-a");
    store.rememberDraftKey("project-a", projectKey);
    expect(store.deleteActivityGroup("project-a")).toEqual([projectKey]);
    expect(resumeOrCreateChatDraftKey(store, "project-a", () => nextProjectKey)).toBe(
      nextProjectKey
    );
  });

  test("project deletion clears an unmaterialized slot without touching other scopes", () => {
    const store = createChatRuntimeStore<Conversation, Message>();
    const deletedProjectKey = createChatDraftKey("unmaterialized-deleted-project");
    const rootKey = createChatDraftKey("kept-root");
    const otherProjectKey = createChatDraftKey("kept-other-project");
    const replacementKey = createChatDraftKey("replacement-deleted-project");
    const rootComposer = createChatComposerState();
    rootComposer.input = "root draft";
    const otherComposer = createChatComposerState("project-b");
    otherComposer.input = "other project draft";

    store.rememberDraftKey("project-a", deletedProjectKey);
    store.ensure(rootKey, { composer: rootComposer });
    store.rememberDraftKey(null, rootKey);
    store.ensure(otherProjectKey, { composer: otherComposer });
    store.rememberDraftKey("project-b", otherProjectKey);

    expect(store.deleteActivityGroup("project-a")).toEqual([]);
    expect(resumeOrCreateChatDraftKey(store, "project-a", () => replacementKey)).toBe(
      replacementKey
    );
    expect(resumeOrCreateChatDraftKey(store, null)).toBe(rootKey);
    expect(resumeOrCreateChatDraftKey(store, "project-b")).toBe(otherProjectKey);
  });

  test("landing after project deletion restores an unrelated retained root draft", () => {
    const store = createChatRuntimeStore<Conversation, Message>();
    const rootKey = createChatDraftKey("root-before-project-deletion");
    const projectKey = createChatDraftKey("deleted-project-draft");
    const unexpectedReplacementKey = createChatDraftKey("unexpected-root-replacement");
    const rootComposer = createChatComposerState();
    rootComposer.input = "keep this root draft";

    store.ensure(rootKey, { composer: rootComposer });
    store.rememberDraftKey(null, rootKey);
    store.ensure(projectKey, { composer: createChatComposerState("project-a") });
    store.rememberDraftKey("project-a", projectKey);
    store.updateActivityGroup(projectKey, "project-a");

    expect(store.deleteActivityGroup("project-a")).toEqual([projectKey]);
    expect(rootChatDraftKeyAfterProjectDeletion(store, () => unexpectedReplacementKey)).toBe(
      rootKey
    );
    expect(store.getRememberedDraftKey(null)).toBe(rootKey);
    expect(store.get(rootKey)?.composer.input).toBe("keep this root draft");
    expect(store.get(unexpectedReplacementKey)).toBeUndefined();
  });

  test("remembered drafts and resources stay isolated across separate store instances", () => {
    const revokeObjectURL = spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    const firstAccountStore = createChatRuntimeStore<Conversation, Message>();
    const secondAccountStore = createChatRuntimeStore<Conversation, Message>();
    const firstAccountKey = createChatDraftKey("first-account");
    const secondAccountKey = createChatDraftKey("second-account");
    const image = new File(["private"], "private.png", { type: "image/png" });
    const queuedImage = new File(["queued-private"], "queued-private.png", {
      type: "image/png"
    });
    const composer = createChatComposerState();
    composer.input = "first account draft";
    composer.draftImages = [image];
    composer.imageUrls.set(image, "blob:first-account");
    composer.queue.items = [
      queuedMessage("first-account-queued", {
        draftImages: [queuedImage],
        imageUrls: new Map([[queuedImage, "blob:first-account-queued"]])
      })
    ];

    try {
      firstAccountStore.select(firstAccountKey, { composer });
      firstAccountStore.rememberDraftKey(null, firstAccountKey);
      const run = firstAccountStore.beginRun(firstAccountKey);

      expect(resumeOrCreateChatDraftKey(secondAccountStore, null, () => secondAccountKey)).toBe(
        secondAccountKey
      );

      firstAccountStore.dispose();

      expect(run.signal.aborted).toBe(true);
      expect(revokeObjectURL).toHaveBeenCalledWith("blob:first-account");
      expect(revokeObjectURL).toHaveBeenCalledWith("blob:first-account-queued");
      expect(secondAccountStore.getRememberedDraftKey(null)).toBe(secondAccountKey);
    } finally {
      firstAccountStore.dispose();
      secondAccountStore.dispose();
      revokeObjectURL.mockRestore();
    }
  });

  test("evicts a completed rekeyed conversation even if draft project metadata is stale", () => {
    const store = createChatRuntimeStore<Conversation, Message>(0);
    const draftKey = createChatDraftKey("project-draft");
    const conversationKey = createConversationChatKey("created-conversation");
    const nextActiveKey = createConversationChatKey("next-active");
    store.select(draftKey, {
      composer: createChatComposerState("project-a")
    });
    const run = store.beginRun(draftKey);

    expect(store.rekey(draftKey, conversationKey, run.token)).toBe(conversationKey);
    expect(store.finishRun(conversationKey, run.token)).toBe(true);
    expect(store.get(conversationKey)?.composer.draftProjectId).toBe("project-a");

    store.select(nextActiveKey);

    expect(store.get(conversationKey)).toBeUndefined();
    expect(store.get(draftKey)).toBeUndefined();
    expect(store.getActiveKey()).toBe(nextActiveKey);
  });

  test("deleting an idle superseded draft disposes only its object URLs", () => {
    const revokeObjectURL = spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    const store = createChatRuntimeStore<Conversation, Message>();
    const idleDraftKey = createChatDraftKey("idle-superseded");
    const runningDraftKey = createChatDraftKey("still-running");
    const idleFile = new File(["idle"], "idle.png", { type: "image/png" });
    const runningFile = new File(["running"], "running.png", { type: "image/png" });
    const idleComposer = createChatComposerState();
    idleComposer.draftImages = [idleFile];
    idleComposer.imageUrls.set(idleFile, "blob:idle");
    const runningComposer = createChatComposerState();
    runningComposer.draftImages = [runningFile];
    runningComposer.imageUrls.set(runningFile, "blob:running");

    try {
      store.ensure(idleDraftKey, { composer: idleComposer });
      store.select(runningDraftKey, { composer: runningComposer });
      const running = store.beginRun(runningDraftKey);

      expect(store.delete(idleDraftKey)).toBe(true);

      expect(revokeObjectURL).toHaveBeenCalledTimes(1);
      expect(revokeObjectURL).toHaveBeenCalledWith("blob:idle");
      expect(store.get(idleDraftKey)).toBeUndefined();
      expect(store.isRunCurrent(runningDraftKey, running.token)).toBe(true);
      expect(running.signal.aborted).toBe(false);
      expect(store.get(runningDraftKey)?.composer.imageUrls.get(runningFile)).toBe("blob:running");

      store.dispose();
      expect(running.signal.aborted).toBe(true);
      expect(revokeObjectURL).toHaveBeenCalledWith("blob:running");
    } finally {
      store.dispose();
      revokeObjectURL.mockRestore();
    }
  });

  test("disposal revokes active and queued object URLs exactly once", () => {
    const revokeObjectURL = spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    const store = createChatRuntimeStore<Conversation, Message>();
    const key = createChatDraftKey("queued-object-urls");
    const activeFile = new File(["active"], "active.png", { type: "image/png" });
    const queuedFile = new File(["queued"], "queued.png", { type: "image/png" });
    const composer = createChatComposerState();
    composer.draftImages = [activeFile];
    composer.imageUrls.set(activeFile, "blob:shared");
    composer.queue.items = [
      queuedMessage("with-images", {
        draftImages: [queuedFile],
        imageUrls: new Map([
          [activeFile, "blob:shared"],
          [queuedFile, "blob:queued"]
        ])
      })
    ];

    try {
      store.ensure(key, { composer });
      store.dispose();

      expect(revokeObjectURL).toHaveBeenCalledTimes(2);
      expect(revokeObjectURL).toHaveBeenCalledWith("blob:shared");
      expect(revokeObjectURL).toHaveBeenCalledWith("blob:queued");
    } finally {
      store.dispose();
      revokeObjectURL.mockRestore();
    }
  });

  test("draft adoption merges source and destination queues without losing composer text", () => {
    const store = createChatRuntimeStore<Conversation, Message>();
    const sourceKey = createChatDraftKey("queued-source");
    const destinationKey = createConversationChatKey("queued-destination");
    const sourceComposer = createChatComposerState("project-a");
    sourceComposer.input = "source draft";
    sourceComposer.queue.items = [queuedMessage("source", { createdMs: 20 })];
    const destinationComposer = createChatComposerState();
    destinationComposer.input = "destination draft";
    destinationComposer.queue.items = [queuedMessage("destination", { createdMs: 10 })];

    store.select(sourceKey, { composer: sourceComposer });
    const run = store.beginRun(sourceKey);
    store.ensure(destinationKey, {
      conversation: { id: "queued-destination" },
      composer: destinationComposer,
      historyLoaded: true
    });

    expect(
      store.rekeyRunAdoptingIdleDestination(
        sourceKey,
        destinationKey,
        run.token,
        (source, destination) => ({
          ...source,
          conversation: destination.conversation,
          historyLoaded: destination.historyLoaded,
          composer: mergeChatComposerDraftsForRekey(
            source.composer,
            destination.composer,
            destinationKey
          ).composer
        })
      )
    ).toMatchObject({ status: "migrated", key: destinationKey });

    expect(store.get(destinationKey)?.composer.input).toBe("destination draft\nsource draft");
    expect(store.get(destinationKey)?.composer.queue.items.map((item) => item.queueId)).toEqual([
      "queue-destination",
      "queue-source"
    ]);
    expect(store.isRunCurrent(destinationKey, run.token)).toBe(true);
    expect(store.get(sourceKey)).toBe(store.get(destinationKey));
  });
});
