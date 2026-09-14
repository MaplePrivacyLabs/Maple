import { describe, expect, test } from "bun:test";
import {
  MAX_CHAT_ACCOUNT_RETAINED_ATTACHMENT_BYTES,
  MAX_CHAT_ACCOUNT_RETAINED_QUEUE_MESSAGES,
  MAX_CHAT_QUEUED_AGGREGATE_ATTACHMENT_BYTES,
  MAX_CHAT_QUEUED_DOCUMENT_BYTES,
  MAX_CHAT_QUEUED_IMAGE_BYTES,
  MAX_CHAT_QUEUED_IMAGES_PER_ITEM,
  MAX_CHAT_QUEUED_MESSAGES,
  MAX_CHAT_QUEUED_TEXT_BYTES,
  beginChatQueuedMessageEdit,
  cancelChatQueuedMessage,
  chatComposerObjectUrls,
  detachChatComposerDraft,
  discardChatQueuedMessageEdit,
  emptyChatComposerQueueState,
  enqueueChatQueuedMessage,
  mergeChatComposerDraftsForRekey,
  queuedChatMessageEditStillPresent,
  recoverDetachedChatComposerDraft,
  stageChatComposerDraft,
  takeNextChatQueuedMessage,
  updateChatQueuedMessage,
  type ChatComposerDraft,
  type ChatComposerQueueState,
  type ChatQueuedMessage
} from "./chatComposerQueue";

function queuedMessage(
  id: string,
  text = id,
  overrides: Partial<ChatQueuedMessage> = {}
): ChatQueuedMessage {
  return {
    queueId: `queue-${id}`,
    messageId: `message-${id}`,
    text,
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

function composer(input = "", overrides: Partial<ChatComposerDraft> = {}): ChatComposerDraft {
  return {
    input,
    draftImages: [],
    imageUrls: new Map(),
    documentText: "",
    documentName: "",
    draftProjectId: null,
    isProcessingDocument: false,
    imagePasteGeneration: 0,
    documentUploadGeneration: 0,
    queue: emptyChatComposerQueueState(),
    ...overrides
  };
}

function queueWith(...items: ChatQueuedMessage[]): ChatComposerQueueState {
  return { items, edit: null };
}

function imageWithSize(name: string, size: number): File {
  const file = new File([], name, { type: "image/png" });
  Object.defineProperty(file, "size", { value: size });
  return file;
}

describe("chat composer queue", () => {
  test("takes messages in FIFO order", () => {
    let queue = emptyChatComposerQueueState();
    for (const item of [queuedMessage("first"), queuedMessage("second")]) {
      const result = enqueueChatQueuedMessage(queue, item);
      expect(result.status).toBe("enqueued");
      if (result.status === "enqueued") queue = result.queue;
    }

    const first = takeNextChatQueuedMessage(queue);
    expect(first.status).toBe("taken");
    if (first.status !== "taken") throw new Error("expected the first queued message");
    expect(first.item.queueId).toBe("queue-first");

    const second = takeNextChatQueuedMessage(first.queue);
    expect(second.status).toBe("taken");
    if (second.status !== "taken") throw new Error("expected the second queued message");
    expect(second.item.queueId).toBe("queue-second");
    expect(takeNextChatQueuedMessage(second.queue).status).toBe("empty");
  });

  test("enforces the normal item limit and UTF-8 staged-text limit", () => {
    let queue = emptyChatComposerQueueState();
    for (let index = 0; index < MAX_CHAT_QUEUED_MESSAGES; index += 1) {
      const result = enqueueChatQueuedMessage(queue, queuedMessage(String(index)));
      expect(result.status).toBe("enqueued");
      if (result.status === "enqueued") queue = result.queue;
    }

    const full = enqueueChatQueuedMessage(queue, queuedMessage("overflow"));
    expect(full.status).toBe("queue_full");
    expect(full.queue).toBe(queue);

    const exactUtf8Limit = "é".repeat(MAX_CHAT_QUEUED_TEXT_BYTES / 2);
    expect(
      enqueueChatQueuedMessage(
        emptyChatComposerQueueState(),
        queuedMessage("exact", exactUtf8Limit)
      ).status
    ).toBe("enqueued");
    expect(
      enqueueChatQueuedMessage(
        emptyChatComposerQueueState(),
        queuedMessage("too-large", `${exactUtf8Limit}a`)
      ).status
    ).toBe("text_too_large");
  });

  test("enforces account-wide queued-message and retained-attachment budgets", () => {
    const queue = emptyChatComposerQueueState();
    const candidate = queuedMessage("account-candidate");
    expect(
      enqueueChatQueuedMessage(queue, candidate, {
        queuedMessageCount: MAX_CHAT_ACCOUNT_RETAINED_QUEUE_MESSAGES,
        attachmentBytes: 0,
        imageCount: 0
      }).status
    ).toBe("account_queue_full");
    expect(
      enqueueChatQueuedMessage(queue, candidate, {
        queuedMessageCount: 0,
        attachmentBytes: MAX_CHAT_ACCOUNT_RETAINED_ATTACHMENT_BYTES + 1,
        imageCount: 0
      }).status
    ).toBe("account_payload_too_large");
  });

  test("enforces per-item image and UTF-8 document payload limits", () => {
    const exactImages = Array.from({ length: MAX_CHAT_QUEUED_IMAGES_PER_ITEM }, (_, index) =>
      imageWithSize(`exact-count-${index}.png`, 1)
    );
    expect(
      enqueueChatQueuedMessage(
        emptyChatComposerQueueState(),
        queuedMessage("exact-image-count", "", { draftImages: exactImages })
      ).status
    ).toBe("enqueued");
    expect(
      enqueueChatQueuedMessage(
        emptyChatComposerQueueState(),
        queuedMessage("too-many-images", "", {
          draftImages: [...exactImages, imageWithSize("one-too-many.png", 1)]
        })
      ).status
    ).toBe("too_many_images");
    const mapOnlyImages = Array.from({ length: MAX_CHAT_QUEUED_IMAGES_PER_ITEM + 1 }, (_, index) =>
      imageWithSize(`map-only-${index}.png`, 1)
    );
    expect(
      enqueueChatQueuedMessage(
        emptyChatComposerQueueState(),
        queuedMessage("too-many-map-owned-images", "text", {
          imageUrls: new Map(mapOnlyImages.map((image) => [image, `blob:${image.name}`]))
        })
      ).status
    ).toBe("too_many_images");

    expect(
      enqueueChatQueuedMessage(
        emptyChatComposerQueueState(),
        queuedMessage("exact-image-size", "", {
          draftImages: [imageWithSize("exact-size.png", MAX_CHAT_QUEUED_IMAGE_BYTES)]
        })
      ).status
    ).toBe("enqueued");
    expect(
      enqueueChatQueuedMessage(
        emptyChatComposerQueueState(),
        queuedMessage("large-image", "", {
          draftImages: [imageWithSize("too-large.png", MAX_CHAT_QUEUED_IMAGE_BYTES + 1)]
        })
      ).status
    ).toBe("image_too_large");

    const exactDocument = "é".repeat(MAX_CHAT_QUEUED_DOCUMENT_BYTES / 2);
    expect(
      enqueueChatQueuedMessage(
        emptyChatComposerQueueState(),
        queuedMessage("exact-document", "", { documentText: exactDocument })
      ).status
    ).toBe("enqueued");
    expect(
      enqueueChatQueuedMessage(
        emptyChatComposerQueueState(),
        queuedMessage("large-document", "", { documentText: `${exactDocument}d` })
      ).status
    ).toBe("document_too_large");
  });

  test("accepts the exact aggregate attachment limit and rejects one byte more", () => {
    const mebibyte = 1024 * 1024;
    const existing = queuedMessage("aggregate-existing", "", {
      draftImages: Array.from({ length: MAX_CHAT_QUEUED_IMAGES_PER_ITEM }, (_, index) =>
        imageWithSize(`existing-${index}.png`, MAX_CHAT_QUEUED_IMAGE_BYTES)
      ),
      documentText: "d".repeat(MAX_CHAT_QUEUED_DOCUMENT_BYTES)
    });
    const initialQueue = queueWith(existing);
    const exactCandidate = queuedMessage("aggregate-exact", "", {
      draftImages: [
        imageWithSize("exact-a.png", 18 * mebibyte),
        imageWithSize("exact-b.png", 18 * mebibyte)
      ],
      documentText: "d".repeat(10 * mebibyte)
    });

    expect(
      10 * MAX_CHAT_QUEUED_IMAGE_BYTES +
        MAX_CHAT_QUEUED_DOCUMENT_BYTES +
        36 * mebibyte +
        10 * mebibyte
    ).toBe(MAX_CHAT_QUEUED_AGGREGATE_ATTACHMENT_BYTES);
    expect(enqueueChatQueuedMessage(initialQueue, exactCandidate).status).toBe("enqueued");

    const overCandidate = queuedMessage("aggregate-over", "", {
      draftImages: [
        imageWithSize("over-a.png", 18 * mebibyte),
        imageWithSize("over-b.png", 18 * mebibyte + 1)
      ],
      documentText: "d".repeat(10 * mebibyte)
    });
    expect(enqueueChatQueuedMessage(initialQueue, overCandidate)).toMatchObject({
      status: "queue_payload_too_large",
      queue: initialQueue
    });
  });

  test("holds FIFO promotion during an edit and restores the stashed text", () => {
    const first = queuedMessage("first", "first text");
    const second = queuedMessage("second", "second text");
    const started = beginChatQueuedMessageEdit(
      queueWith(first, second),
      "conversation:one",
      second.queueId,
      "new draft"
    );

    expect(started.status).toBe("started");
    if (started.status !== "started") throw new Error("expected edit to start");
    expect(started.input).toBe("second text");
    expect(started.queue.edit).toEqual({
      scopeKey: "conversation:one",
      queueId: second.queueId,
      stashedDraft: "new draft"
    });
    expect(queuedChatMessageEditStillPresent(started.queue)).toBe(true);
    expect(takeNextChatQueuedMessage(started.queue).status).toBe("blocked_by_edit");

    const updated = updateChatQueuedMessage(started.queue, second.queueId, " revised text ");
    expect(updated.status).toBe("updated");
    if (updated.status !== "updated") throw new Error("expected edit to update");
    expect(updated.queue.items.map((item) => item.text)).toEqual(["first text", "revised text"]);
    expect(updated.restoreInput).toBe("new draft");
    expect(updated.queue.edit).toBeNull();
    expect(takeNextChatQueuedMessage(updated.queue)).toMatchObject({
      status: "taken",
      item: { queueId: first.queueId }
    });
  });

  test("queued text edits accept the exact UTF-8 limit and reject one byte more", () => {
    const item = queuedMessage("bounded-edit", "original");
    const edit = {
      scopeKey: "conversation:bounded-edit",
      queueId: item.queueId,
      stashedDraft: "draft"
    };
    const queue = { items: [item], edit };
    const exact = "é".repeat(MAX_CHAT_QUEUED_TEXT_BYTES / 2);

    expect(updateChatQueuedMessage(queue, item.queueId, exact).status).toBe("updated");
    expect(updateChatQueuedMessage(queue, item.queueId, `${exact}a`).status).toBe("text_too_large");
  });

  test("switching edits keeps the original stash and cancel restores it", () => {
    const first = queuedMessage("first", "first text");
    const second = queuedMessage("second", "second text");
    const initial = beginChatQueuedMessageEdit(
      queueWith(first, second),
      "conversation:one",
      first.queueId,
      "original draft"
    );
    if (initial.status !== "started") throw new Error("expected initial edit");
    const switched = beginChatQueuedMessageEdit(
      initial.queue,
      "conversation:one",
      second.queueId,
      initial.input
    );
    if (switched.status !== "started") throw new Error("expected switched edit");
    expect(switched.queue.edit?.stashedDraft).toBe("original draft");

    const cancelled = cancelChatQueuedMessage(switched.queue, second.queueId);
    expect(cancelled.status).toBe("cancelled");
    if (cancelled.status !== "cancelled") throw new Error("expected cancel");
    expect(cancelled.restoreInput).toBe("original draft");
    expect(cancelled.queue.items).toEqual([first]);
    expect(cancelled.queue.edit).toBeNull();
  });

  test("discarding an edit restores its draft without changing queue order", () => {
    const item = queuedMessage("edit", "queued text");
    const started = beginChatQueuedMessageEdit(
      queueWith(item),
      "conversation:one",
      item.queueId,
      "stashed"
    );
    if (started.status !== "started") throw new Error("expected edit");

    expect(discardChatQueuedMessageEdit(started.queue)).toEqual({
      status: "ended",
      queue: queueWith(item),
      restoreInput: "stashed"
    });
  });

  test("detaches every send-owned field and stages without mutating the source draft", () => {
    const image = new File(["image"], "draft.png", { type: "image/png" });
    const original = composer("  prompt with context  ", {
      draftImages: [image],
      imageUrls: new Map([[image, "blob:draft-image"]]),
      documentText: "document payload",
      documentName: "notes.md",
      draftProjectId: "project-a",
      imagePasteGeneration: 4,
      documentUploadGeneration: 8
    });
    const metadata = {
      queueId: "queue-detached",
      messageId: "message-detached",
      model: "model-at-submit",
      webSearchEnabled: true,
      createdMs: 42
    };

    const detached = detachChatComposerDraft(original, metadata);
    expect(detached.item).toMatchObject({
      ...metadata,
      text: "prompt with context",
      documentText: "document payload",
      documentName: "notes.md",
      draftProjectId: "project-a"
    });
    expect(detached.item.draftImages).toEqual([image]);
    expect(detached.item.imageUrls.get(image)).toBe("blob:draft-image");
    expect(detached.composer).toMatchObject({
      input: "",
      draftImages: [],
      documentText: "",
      documentName: "",
      draftProjectId: "project-a",
      imagePasteGeneration: 5,
      documentUploadGeneration: 9
    });
    expect(detached.composer.imageUrls.size).toBe(0);
    expect(original.input).toBe("  prompt with context  ");
    expect(original.draftImages).toEqual([image]);
    expect(original.imageUrls.get(image)).toBe("blob:draft-image");

    const staged = stageChatComposerDraft(original, metadata);
    expect(staged.status).toBe("enqueued");
    if (staged.status !== "enqueued") throw new Error("expected staged draft");
    expect(staged.composer.queue.items).toEqual([staged.item]);
    expect(staged.item.model).toBe("model-at-submit");
    expect(staged.item.webSearchEnabled).toBe(true);
  });

  test("a rejected stage returns the exact draft and leaves its resources attached", () => {
    const image = new File(["image"], "kept.png", { type: "image/png" });
    const fullQueue = queueWith(
      ...Array.from({ length: MAX_CHAT_QUEUED_MESSAGES }, (_, index) =>
        queuedMessage(`existing-${index}`)
      )
    );
    const original = composer("keep me", {
      draftImages: [image],
      imageUrls: new Map([[image, "blob:kept"]]),
      queue: fullQueue
    });
    const result = stageChatComposerDraft(original, {
      queueId: "queue-rejected",
      messageId: "message-rejected",
      model: "model",
      webSearchEnabled: false,
      createdMs: 1
    });

    expect(result.status).toBe("queue_full");
    expect(result.composer).toBe(original);
    expect(result.composer.imageUrls.get(image)).toBe("blob:kept");
  });

  test("stage propagates attachment-limit failures without detaching the draft", () => {
    const images = Array.from({ length: MAX_CHAT_QUEUED_IMAGES_PER_ITEM + 1 }, (_, index) =>
      imageWithSize(`stage-${index}.png`, 1)
    );
    const original = composer("keep staged payload", { draftImages: images });
    const result = stageChatComposerDraft(original, {
      queueId: "queue-attachment-rejected",
      messageId: "message-attachment-rejected",
      model: "model",
      webSearchEnabled: false,
      createdMs: 1
    });

    expect(result.status).toBe("too_many_images");
    expect(result.composer).toBe(original);
    expect(result.composer.draftImages).toBe(images);
  });

  test("recovers failed detached sends in place or in a temporary seventeenth slot", () => {
    const image = new File(["image"], "failed.png", { type: "image/png" });
    const failed = queuedMessage("failed", "failed prompt", {
      draftImages: [image],
      imageUrls: new Map([[image, "blob:failed"]]),
      documentText: "failed document",
      documentName: "failed.md",
      draftProjectId: "project-a"
    });

    const restored = recoverDetachedChatComposerDraft(composer(), failed);
    expect(restored.status).toBe("restored");
    expect(restored.composer).toMatchObject({
      input: "failed prompt",
      documentText: "failed document",
      documentName: "failed.md",
      draftProjectId: "project-a"
    });
    expect(restored.composer.draftImages).toEqual([image]);
    expect(restored.composer.imageUrls.get(image)).toBe("blob:failed");

    const fullQueue = queueWith(
      ...Array.from({ length: MAX_CHAT_QUEUED_MESSAGES }, (_, index) =>
        queuedMessage(`existing-${index}`)
      )
    );
    const requeued = recoverDetachedChatComposerDraft(
      composer("a newer draft", { queue: fullQueue }),
      failed
    );
    expect(requeued.status).toBe("requeued");
    expect(requeued.composer.queue.items).toHaveLength(MAX_CHAT_QUEUED_MESSAGES + 1);
    expect(requeued.composer.queue.items[0]).toBe(failed);
    expect(
      enqueueChatQueuedMessage(
        requeued.composer.queue,
        queuedMessage("normal-enqueue-remains-blocked")
      ).status
    ).toBe("queue_full");
    expect(recoverDetachedChatComposerDraft(requeued.composer, failed).status).toBe(
      "already_queued"
    );
  });

  test("recovery and rekey remain lossless over aggregate bounds but block new admission", () => {
    const existing = queuedMessage("recovery-existing", "", {
      draftImages: Array.from({ length: 10 }, (_, index) =>
        imageWithSize(`recovery-existing-${index}.png`, MAX_CHAT_QUEUED_IMAGE_BYTES)
      ),
      documentText: "d".repeat(MAX_CHAT_QUEUED_DOCUMENT_BYTES)
    });
    const failed = queuedMessage("recovery-failed", "", {
      draftImages: Array.from({ length: 3 }, (_, index) =>
        imageWithSize(`recovery-failed-${index}.png`, MAX_CHAT_QUEUED_IMAGE_BYTES)
      )
    });
    const recovered = recoverDetachedChatComposerDraft(
      composer("newer draft", { queue: queueWith(existing) }),
      failed
    );

    expect(recovered.status).toBe("requeued");
    expect(recovered.composer.queue.items).toEqual([failed, existing]);
    expect(
      enqueueChatQueuedMessage(recovered.composer.queue, queuedMessage("blocked-after-recovery"))
        .status
    ).toBe("queue_payload_too_large");

    const sourceItem = queuedMessage("oversized-source", "", {
      draftImages: Array.from({ length: 7 }, (_, index) =>
        imageWithSize(`source-${index}.png`, MAX_CHAT_QUEUED_IMAGE_BYTES)
      )
    });
    const destinationItem = queuedMessage("oversized-destination", "", {
      draftImages: Array.from({ length: 7 }, (_, index) =>
        imageWithSize(`destination-${index}.png`, MAX_CHAT_QUEUED_IMAGE_BYTES)
      )
    });
    const merged = mergeChatComposerDraftsForRekey(
      composer("", { queue: queueWith(sourceItem) }),
      composer("", { queue: queueWith(destinationItem) }),
      "conversation:oversized"
    );

    expect(merged.composer.queue.items).toHaveLength(2);
    expect(merged.composer.queue.items).toContain(sourceItem);
    expect(merged.composer.queue.items).toContain(destinationItem);
    expect(
      enqueueChatQueuedMessage(merged.composer.queue, queuedMessage("blocked-after-rekey")).status
    ).toBe("queue_payload_too_large");
  });

  test("rekey preserves a source-only edit as the sole promotion fence", () => {
    const sourceItem = queuedMessage("source-edit", "original queued text", {
      queueId: "collision",
      messageId: "message-collision",
      createdMs: 10
    });
    const destinationItem = queuedMessage("destination", "destination queued text", {
      queueId: "collision",
      messageId: "message-collision",
      createdMs: 20
    });
    const oversizedEdit = "x".repeat(MAX_CHAT_QUEUED_TEXT_BYTES + 1);
    const source = composer(oversizedEdit, {
      queue: {
        items: [sourceItem],
        edit: {
          scopeKey: "draft:source",
          queueId: sourceItem.queueId,
          stashedDraft: "source stashed draft"
        }
      }
    });
    const destination = composer("destination draft", {
      queue: queueWith(destinationItem)
    });

    const merged = mergeChatComposerDraftsForRekey(source, destination, "conversation:created");

    expect(merged.composer.input).toBe(oversizedEdit);
    expect(merged.composer.queue.edit).toEqual({
      scopeKey: "conversation:created",
      queueId: "collision:rekey:2",
      stashedDraft: "destination draft\nsource stashed draft"
    });
    expect(
      merged.composer.queue.items.find((item) => item.queueId === "collision:rekey:2")?.text
    ).toBe("original queued text");
    expect(takeNextChatQueuedMessage(merged.composer.queue).status).toBe("blocked_by_edit");
    expect(
      updateChatQueuedMessage(
        merged.composer.queue,
        merged.composer.queue.edit!.queueId,
        merged.composer.input
      ).status
    ).toBe("text_too_large");
  });

  test("rekey merge preserves both drafts and queues while the destination edit stays visible", () => {
    const sharedImage = new File(["shared"], "shared.png", { type: "image/png" });
    const sourceOnlyImage = new File(["source"], "source.png", { type: "image/png" });
    const destinationCollision = queuedMessage("destination-collision", "destination item", {
      queueId: "collision",
      messageId: "message-collision",
      createdMs: 20,
      draftProjectId: "destination-project"
    });
    const destinationEditItem = queuedMessage("destination-edit", "destination queued", {
      createdMs: 40
    });
    const sourceCollision = queuedMessage("source-collision", "source queued before edit", {
      queueId: "collision",
      messageId: "message-collision",
      createdMs: 10,
      draftProjectId: "source-project"
    });
    const sourceLater = queuedMessage("source-later", "source later", { createdMs: 30 });
    const source = composer("source edited queue text", {
      draftImages: [sharedImage, sourceOnlyImage],
      imageUrls: new Map([
        [sharedImage, "blob:source-shared"],
        [sourceOnlyImage, "blob:source-only"]
      ]),
      documentText: "source document",
      documentName: "source.md",
      draftProjectId: "source-project",
      queue: {
        items: [sourceCollision, sourceLater],
        edit: {
          scopeKey: "draft:source",
          queueId: sourceCollision.queueId,
          stashedDraft: "source stashed draft"
        }
      }
    });
    const destination = composer("destination edited queue text", {
      draftImages: [sharedImage],
      imageUrls: new Map([[sharedImage, "blob:destination-shared"]]),
      documentText: "destination document",
      documentName: "destination.md",
      draftProjectId: "destination-project",
      queue: {
        items: [destinationCollision, destinationEditItem],
        edit: {
          scopeKey: "conversation:destination",
          queueId: destinationEditItem.queueId,
          stashedDraft: "destination stashed draft"
        }
      }
    });

    const merged = mergeChatComposerDraftsForRekey(source, destination, "conversation:merged");

    expect(merged.composer.input).toBe("destination edited queue text");
    expect(merged.composer.queue.edit).toEqual({
      scopeKey: "conversation:merged",
      queueId: destinationEditItem.queueId,
      stashedDraft: "destination stashed draft\nsource stashed draft\nsource edited queue text"
    });
    expect(merged.composer.queue.items).toHaveLength(4);
    expect(merged.composer.queue.items.map((item) => item.createdMs)).toEqual([10, 20, 30, 40]);
    expect(new Set(merged.composer.queue.items.map((item) => item.queueId)).size).toBe(4);
    expect(new Set(merged.composer.queue.items.map((item) => item.messageId)).size).toBe(4);
    expect(merged.composer.queue.items.find((item) => item.createdMs === 10)?.text).toBe(
      "source queued before edit"
    );
    expect(merged.composer.queue.items.find((item) => item.createdMs === 10)?.draftProjectId).toBe(
      "source-project"
    );
    expect(merged.composer.draftImages).toEqual([sharedImage, sourceOnlyImage]);
    expect(merged.composer.imageUrls.get(sharedImage)).toBe("blob:destination-shared");
    expect(merged.composer.imageUrls.get(sourceOnlyImage)).toBe("blob:source-only");
    expect(merged.displacedObjectUrls).toEqual(["blob:source-shared"]);
    expect(merged.composer.documentText).toBe("destination document\n\nsource document");
    expect(merged.composer.documentName).toBe("destination.md, source.md");
    expect(merged.composer.draftProjectId).toBe("destination-project");
    expect(takeNextChatQueuedMessage(merged.composer.queue).status).toBe("blocked_by_edit");

    const resolved = updateChatQueuedMessage(
      merged.composer.queue,
      destinationEditItem.queueId,
      merged.composer.input
    );
    expect(resolved.status).toBe("updated");
    if (resolved.status !== "updated") throw new Error("expected explicit edit resolution");
    expect(resolved.restoreInput).toBe(
      "destination stashed draft\nsource stashed draft\nsource edited queue text"
    );
    expect(resolved.queue.items.some((item) => item.text === "source edited queue text")).toBe(
      false
    );
    expect(takeNextChatQueuedMessage(resolved.queue).status).toBe("taken");
  });

  test("collects active and queued object URLs once", () => {
    const active = new File(["active"], "active.png", { type: "image/png" });
    const queued = new File(["queued"], "queued.png", { type: "image/png" });
    const draft = composer("", {
      draftImages: [active],
      imageUrls: new Map([[active, "blob:shared"]]),
      queue: queueWith(
        queuedMessage("queued", "", {
          draftImages: [queued],
          imageUrls: new Map([
            [queued, "blob:shared"],
            [active, "blob:queued-only"]
          ])
        })
      )
    });

    expect(chatComposerObjectUrls(draft)).toEqual(["blob:shared", "blob:queued-only"]);
  });

  test("does not displace an active URL that a merged queued item still owns", () => {
    const sharedActiveFile = new File(["active"], "active.png", { type: "image/png" });
    const queuedFile = new File(["queued"], "queued.png", { type: "image/png" });
    const source = composer("", {
      draftImages: [sharedActiveFile],
      imageUrls: new Map([[sharedActiveFile, "blob:source-active"]]),
      queue: queueWith(
        queuedMessage("retains-url", "", {
          draftImages: [queuedFile],
          imageUrls: new Map([[queuedFile, "blob:source-active"]])
        })
      )
    });
    const destination = composer("", {
      draftImages: [sharedActiveFile],
      imageUrls: new Map([[sharedActiveFile, "blob:destination-active"]])
    });

    const merged = mergeChatComposerDraftsForRekey(source, destination, "conversation:destination");

    expect(merged.composer.imageUrls.get(sharedActiveFile)).toBe("blob:destination-active");
    expect(chatComposerObjectUrls(merged.composer)).toContain("blob:source-active");
    expect(merged.displacedObjectUrls).toEqual([]);
  });
});
