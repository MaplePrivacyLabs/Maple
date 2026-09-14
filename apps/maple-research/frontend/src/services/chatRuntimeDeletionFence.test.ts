import { describe, expect, test } from "bun:test";
import {
  beginAllChatRuntimeDeletionFence,
  beginChatActivityGroupDeletionFence,
  beginChatProjectRuntimeDeletionFence,
  beginChatRuntimeDeletionFence,
  isChatRuntimeDeletionPending
} from "./chatRuntimeDeletionFence";
import type { ChatRuntimeKey } from "./chatRuntimeStore";

function lookup() {
  const aliases = new Map<ChatRuntimeKey, ChatRuntimeKey>();
  const groups = new Map<ChatRuntimeKey, string | null>();
  return {
    aliases,
    groups,
    resolveKey(key: ChatRuntimeKey) {
      return aliases.get(key) ?? key;
    },
    getActivityGroupId(key: ChatRuntimeKey) {
      return groups.get(this.resolveKey(key));
    }
  };
}

describe("chat runtime deletion fences", () => {
  test("fences every runtime while all history is being deleted", () => {
    const store = lookup();
    const firstRelease = beginAllChatRuntimeDeletionFence(store);
    const secondRelease = beginAllChatRuntimeDeletionFence(store);
    const key = "conversation:any" as ChatRuntimeKey;

    expect(isChatRuntimeDeletionPending(store, key)).toBe(true);
    firstRelease();
    firstRelease();
    expect(isChatRuntimeDeletionPending(store, key)).toBe(true);
    secondRelease();
    expect(isChatRuntimeDeletionPending(store, key)).toBe(false);
  });

  test("fences one runtime through rekey and releases it after deletion settles", () => {
    const store = lookup();
    const draftKey = "draft:deleting" as ChatRuntimeKey;
    const conversationKey = "conversation:deleting" as ChatRuntimeKey;
    const release = beginChatRuntimeDeletionFence(store, draftKey);

    expect(isChatRuntimeDeletionPending(store, draftKey)).toBe(true);
    store.aliases.set(draftKey, conversationKey);
    expect(isChatRuntimeDeletionPending(store, conversationKey)).toBe(true);

    release();
    expect(isChatRuntimeDeletionPending(store, conversationKey)).toBe(false);
  });

  test("keeps one runtime fenced until every overlapping lease releases", () => {
    const store = lookup();
    const key = "conversation:overlapping" as ChatRuntimeKey;
    const firstRelease = beginChatRuntimeDeletionFence(store, key);
    const secondRelease = beginChatRuntimeDeletionFence(store, key);

    expect(isChatRuntimeDeletionPending(store, key)).toBe(true);
    firstRelease();
    firstRelease();
    expect(isChatRuntimeDeletionPending(store, key)).toBe(true);
    secondRelease();
    expect(isChatRuntimeDeletionPending(store, key)).toBe(false);
  });

  test("fences every runtime in a deleting activity group", () => {
    const store = lookup();
    const first = "conversation:first" as ChatRuntimeKey;
    const second = "draft:second" as ChatRuntimeKey;
    const unrelated = "conversation:unrelated" as ChatRuntimeKey;
    store.groups.set(first, "project:deleting");
    store.groups.set(second, "project:deleting");
    store.groups.set(unrelated, "project:other");
    const release = beginChatActivityGroupDeletionFence(store, "project:deleting");

    expect(isChatRuntimeDeletionPending(store, first)).toBe(true);
    expect(isChatRuntimeDeletionPending(store, second)).toBe(true);
    expect(isChatRuntimeDeletionPending(store, unrelated)).toBe(false);

    release();
    expect(isChatRuntimeDeletionPending(store, first)).toBe(false);
  });

  test("keeps an activity group fenced until every overlapping lease releases", () => {
    const store = lookup();
    const key = "conversation:project-overlap" as ChatRuntimeKey;
    store.groups.set(key, "project:overlapping");
    const firstRelease = beginChatActivityGroupDeletionFence(store, "project:overlapping");
    const secondRelease = beginChatActivityGroupDeletionFence(store, "project:overlapping");

    expect(isChatRuntimeDeletionPending(store, key)).toBe(true);
    firstRelease();
    firstRelease();
    expect(isChatRuntimeDeletionPending(store, key)).toBe(true);
    secondRelease();
    expect(isChatRuntimeDeletionPending(store, key)).toBe(false);
  });

  test("project deletion also fences exact conversations whose group metadata is unavailable", () => {
    const store = lookup();
    const grouped = "conversation:grouped" as ChatRuntimeKey;
    const metadataUnavailable = "conversation:metadata-unavailable" as ChatRuntimeKey;
    const unrelated = "conversation:unrelated" as ChatRuntimeKey;
    store.groups.set(grouped, "project:deleting");
    store.groups.set(metadataUnavailable, null);
    store.groups.set(unrelated, null);

    const release = beginChatProjectRuntimeDeletionFence(store, "project:deleting", [
      metadataUnavailable
    ]);

    expect(isChatRuntimeDeletionPending(store, grouped)).toBe(true);
    expect(isChatRuntimeDeletionPending(store, metadataUnavailable)).toBe(true);
    expect(isChatRuntimeDeletionPending(store, unrelated)).toBe(false);
    release();
    release();
    expect(isChatRuntimeDeletionPending(store, grouped)).toBe(false);
    expect(isChatRuntimeDeletionPending(store, metadataUnavailable)).toBe(false);
  });
});
