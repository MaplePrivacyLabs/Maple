import { describe, expect, test } from "bun:test";
import { chatAccountQueueUsage, selectChatImageFilesForRetention } from "./chatAccountQueueBudget";
import { registerChatCurrentTurn } from "./chatCurrentTurnRegistry";
import {
  MAX_CHAT_ACCOUNT_RETAINED_IMAGES,
  MAX_CHAT_QUEUED_IMAGES_PER_ITEM,
  type ChatAccountQueueUsage,
  type ChatQueuedMessage
} from "./chatComposerQueue";

function item(id: string, file: File, documentText = ""): ChatQueuedMessage {
  return {
    queueId: `queue-${id}`,
    messageId: `message-${id}`,
    text: id,
    draftImages: [file],
    imageUrls: new Map([[file, `blob:${id}`]]),
    documentText,
    documentName: "document.txt",
    draftProjectId: null,
    model: "maple-model",
    webSearchEnabled: false,
    createdMs: 0
  };
}

describe("chat account queue budget", () => {
  test("counts retained drafts and queues across runtimes without double-counting files", () => {
    const sharedFile = new File([new Uint8Array(7)], "shared.png");
    const queued = item("queued", sharedFile, "four");
    const store = {
      getSnapshots: () => [
        {
          composer: {
            draftImages: [sharedFile],
            imageUrls: new Map([[sharedFile, "blob:draft"]]),
            documentText: "abc",
            queue: { items: [queued] }
          }
        },
        {
          composer: {
            draftImages: [],
            imageUrls: new Map<File, string>(),
            documentText: "",
            queue: { items: [] }
          }
        }
      ]
    };

    expect(chatAccountQueueUsage(store)).toEqual({
      queuedMessageCount: 1,
      attachmentBytes: 7 + 3 + 4,
      imageCount: 1
    });
  });

  test("keeps a popped FIFO item reserved and deduplicates it after recovery", () => {
    const file = new File([new Uint8Array(11)], "queued.png");
    const queued = item("active", file, "doc");
    let recoveredItems: ChatQueuedMessage[] = [];
    const store = {
      getSnapshots: () => [
        {
          composer: {
            draftImages: [],
            imageUrls: new Map<File, string>(),
            documentText: "",
            queue: { items: recoveredItems }
          }
        }
      ]
    };
    registerChatCurrentTurn(store, 1, {
      responseRequestStarted: () => false,
      restoreBeforeRequest: () => true,
      retainedPayload: queued,
      countsTowardQueueLimit: true
    });

    expect(chatAccountQueueUsage(store)).toEqual({
      queuedMessageCount: 1,
      attachmentBytes: 14,
      imageCount: 1
    });
    recoveredItems = [queued];
    expect(chatAccountQueueUsage(store)).toEqual({
      queuedMessageCount: 1,
      attachmentBytes: 14,
      imageCount: 1
    });
  });

  test("selects an ordered prefix within the live-message image limit", () => {
    const existingFiles = Array.from(
      { length: MAX_CHAT_QUEUED_IMAGES_PER_ITEM - 1 },
      (_, index) => new File([], `existing-${index}.png`, { type: "image/png" })
    );
    const first = new File([], "first.png", { type: "image/png" });
    const second = new File([], "second.png", { type: "image/png" });
    const accountUsage: ChatAccountQueueUsage = {
      queuedMessageCount: 0,
      attachmentBytes: 0,
      imageCount: existingFiles.length
    };

    const selected = selectChatImageFilesForRetention({
      composer: { draftImages: existingFiles, imageUrls: new Map() },
      candidates: [first, second],
      accountUsage
    });

    expect(selected.files).toEqual([first]);
    expect(selected.messageLimitExceeded).toBe(true);
    expect(selected.accountLimitExceeded).toBe(false);
  });

  test("admits only the remaining account slot, including for zero-byte images", () => {
    const first = new File([], "first.png", { type: "image/png" });
    const second = new File([], "second.png", { type: "image/png" });
    const accountUsage: ChatAccountQueueUsage = {
      queuedMessageCount: 0,
      attachmentBytes: 0,
      imageCount: MAX_CHAT_ACCOUNT_RETAINED_IMAGES - 1
    };

    const selected = selectChatImageFilesForRetention({
      composer: { draftImages: [], imageUrls: new Map() },
      candidates: [first, second],
      accountUsage
    });
    const full = selectChatImageFilesForRetention({
      composer: { draftImages: [], imageUrls: new Map() },
      candidates: [first],
      accountUsage: { ...accountUsage, imageCount: MAX_CHAT_ACCOUNT_RETAINED_IMAGES }
    });

    expect(selected.files).toEqual([first]);
    expect(selected.accountLimitExceeded).toBe(true);
    expect(selected.messageLimitExceeded).toBe(false);
    expect(full.files).toEqual([]);
    expect(full.accountLimitExceeded).toBe(true);
  });

  test("ignores repeated and already-owned File identities", () => {
    const existing = new File([], "existing.png", { type: "image/png" });
    const added = new File([], "added.png", { type: "image/png" });
    const accountUsage: ChatAccountQueueUsage = {
      queuedMessageCount: 0,
      attachmentBytes: 0,
      imageCount: 1
    };

    const selected = selectChatImageFilesForRetention({
      composer: {
        draftImages: [existing],
        imageUrls: new Map([[existing, "blob:existing"]])
      },
      candidates: [existing, existing, added, added],
      accountUsage
    });

    expect(selected).toEqual({
      files: [added],
      accountLimitExceeded: false,
      messageLimitExceeded: false
    });
  });
});
