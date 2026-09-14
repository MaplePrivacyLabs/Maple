import { describe, expect, test } from "bun:test";
import { createChatComposerState } from "@/contexts/ChatRuntimeContext";
import {
  emptyChatComposerQueueState,
  MAX_CHAT_ACCOUNT_RETAINED_QUEUE_MESSAGES,
  MAX_CHAT_QUEUED_MESSAGES,
  type ChatQueuedMessage
} from "./chatComposerQueue";
import {
  canSubmitChatComposer,
  chatComposerWithInputOverride,
  chatComposerShowsStop,
  planChatComposerSubmission
} from "./chatComposerSend";

const metadata = {
  queueId: "queue-new",
  messageId: "message-new",
  model: "test-model",
  webSearchEnabled: false,
  createdMs: 3
};

function queued(queueId: string, createdMs: number, text = queueId): ChatQueuedMessage {
  return {
    queueId,
    messageId: `message-${queueId}`,
    text,
    draftImages: [],
    imageUrls: new Map(),
    documentText: "",
    documentName: "",
    draftProjectId: null,
    model: "test-model",
    webSearchEnabled: false,
    createdMs
  };
}

describe("chat composer submission planning", () => {
  test("retains a voice input override without disturbing queue or attachments", () => {
    const image = new File(["image"], "image.png", { type: "image/png" });
    const composer = {
      ...createChatComposerState(),
      input: "before",
      draftImages: [image],
      queue: {
        items: [queued("one", 1)],
        edit: { scopeKey: "conversation:1", queueId: "one", stashedDraft: "later" }
      }
    };

    const retained = chatComposerWithInputOverride(composer, "before dictated words");

    expect(retained.input).toBe("before dictated words");
    expect(retained.draftImages).toBe(composer.draftImages);
    expect(retained.queue).toBe(composer.queue);
    expect(chatComposerWithInputOverride(retained, undefined)).toBe(retained);
  });

  test("detaches an idle draft before asynchronous send work", () => {
    const composer = { ...createChatComposerState(), input: " first message " };
    const plan = planChatComposerSubmission({ composer, hasActiveRun: false, metadata });

    expect(plan.status).toBe("start");
    if (plan.status !== "start") return;
    expect(plan.item.text).toBe("first message");
    expect(plan.composer.input).toBe("");
    expect(plan.recoverOnFailure).toBe(true);
  });

  test("stages a mid-run draft without starting a second run", () => {
    const composer = { ...createChatComposerState(), input: "follow up" };
    const plan = planChatComposerSubmission({ composer, hasActiveRun: true, metadata });

    expect(plan.status).toBe("queued");
    if (plan.status !== "queued") return;
    expect(plan.composer.input).toBe("");
    expect(plan.composer.queue.items.map((item) => item.text)).toEqual(["follow up"]);
  });

  test("appends the live draft after leftovers and starts the oldest item", () => {
    const composer = {
      ...createChatComposerState(),
      input: "newest",
      queue: { items: [queued("oldest", 1), queued("middle", 2)], edit: null }
    };
    const plan = planChatComposerSubmission({ composer, hasActiveRun: false, metadata });

    expect(plan.status).toBe("start");
    if (plan.status !== "start") return;
    expect(plan.item.queueId).toBe("oldest");
    expect(plan.composer.queue.items.map((item) => item.queueId)).toEqual(["middle", "queue-new"]);
    expect(plan.recoverOnFailure).toBe(false);
  });

  test("resumes a full queue before appending the live draft at the tail", () => {
    const items = Array.from({ length: MAX_CHAT_QUEUED_MESSAGES }, (_, index) =>
      queued(`queued-${index}`, index + 1)
    );
    const composer = {
      ...createChatComposerState(),
      input: "newest",
      queue: { items, edit: null }
    };
    const plan = planChatComposerSubmission({ composer, hasActiveRun: false, metadata });

    expect(plan.status).toBe("start");
    if (plan.status !== "start") return;
    expect(plan.item.queueId).toBe("queued-0");
    expect(plan.composer.queue.items).toHaveLength(MAX_CHAT_QUEUED_MESSAGES);
    expect(plan.composer.queue.items.at(-1)?.queueId).toBe("queue-new");
  });

  test("starts the retained FIFO but leaves the live draft when the account queue is full", () => {
    const composer = {
      ...createChatComposerState(),
      input: "keep this live",
      queue: { items: [queued("oldest", 1)], edit: null }
    };
    const plan = planChatComposerSubmission({
      composer,
      hasActiveRun: false,
      metadata,
      accountUsage: {
        queuedMessageCount: MAX_CHAT_ACCOUNT_RETAINED_QUEUE_MESSAGES,
        attachmentBytes: 0,
        imageCount: 0
      }
    });

    expect(plan.status).toBe("start");
    if (plan.status !== "start") return;
    expect(plan.item.queueId).toBe("oldest");
    expect(plan.composer.input).toBe("keep this live");
    expect(plan.composer.queue.items).toEqual([]);
  });

  test("reserves a recovery slot before starting a direct idle draft", () => {
    const composer = { ...createChatComposerState(), input: "keep this live" };
    const plan = planChatComposerSubmission({
      composer,
      hasActiveRun: false,
      metadata,
      accountUsage: {
        queuedMessageCount: MAX_CHAT_ACCOUNT_RETAINED_QUEUE_MESSAGES,
        attachmentBytes: 0,
        imageCount: 0
      }
    });

    expect(plan.status).toBe("account_queue_full");
    expect(composer.input).toBe("keep this live");
  });

  test("drains a losslessly recovered queue that is already over the local limit", () => {
    const items = Array.from({ length: MAX_CHAT_QUEUED_MESSAGES + 2 }, (_, index) =>
      queued(`recovered-${index}`, index + 1)
    );
    const composer = {
      ...createChatComposerState(),
      input: "keep this editable",
      queue: { items, edit: null }
    };
    const plan = planChatComposerSubmission({ composer, hasActiveRun: false, metadata });

    expect(plan.status).toBe("start");
    if (plan.status !== "start") return;
    expect(plan.item.queueId).toBe("recovered-0");
    expect(plan.composer.input).toBe("keep this editable");
    expect(plan.composer.queue.items).toHaveLength(MAX_CHAT_QUEUED_MESSAGES + 1);
  });

  test("rejects a new mid-run item when the account queue is full", () => {
    const composer = { ...createChatComposerState(), input: "not lost" };
    const plan = planChatComposerSubmission({
      composer,
      hasActiveRun: true,
      metadata,
      accountUsage: {
        queuedMessageCount: MAX_CHAT_ACCOUNT_RETAINED_QUEUE_MESSAGES,
        attachmentBytes: 0,
        imageCount: 0
      }
    });

    expect(plan.status).toBe("account_queue_full");
    expect(composer.input).toBe("not lost");
  });

  test("saves an edit in place and either holds or resumes the FIFO", () => {
    const base = {
      ...createChatComposerState(),
      input: "edited",
      queue: {
        items: [queued("one", 1), queued("two", 2)],
        edit: { scopeKey: "conversation:1", queueId: "two", stashedDraft: "later draft" }
      }
    };

    const held = planChatComposerSubmission({ composer: base, hasActiveRun: true, metadata });
    expect(held.status).toBe("updated");
    if (held.status !== "updated") return;
    expect(held.composer.input).toBe("later draft");
    expect(held.composer.queue.items[1].text).toBe("edited");

    const resumed = planChatComposerSubmission({ composer: base, hasActiveRun: false, metadata });
    expect(resumed.status).toBe("start");
    if (resumed.status !== "start") return;
    expect(resumed.item.queueId).toBe("one");
    expect(resumed.composer.queue.items[0].text).toBe("edited");
  });

  test("empty idle send flushes a retained queue but empty active send does nothing", () => {
    const composer = {
      ...createChatComposerState(),
      queue: { items: [queued("one", 1)], edit: null }
    };
    expect(planChatComposerSubmission({ composer, hasActiveRun: true, metadata }).status).toBe(
      "empty"
    );
    expect(planChatComposerSubmission({ composer, hasActiveRun: false, metadata }).status).toBe(
      "start"
    );
  });

  test("send and Stop policy keeps both controls available during a run", () => {
    expect(
      canSubmitChatComposer({
        text: "queue this",
        hasAttachments: false,
        hasQueuedMessages: true,
        hasActiveRun: true,
        isProcessingDocument: false,
        isStopping: false
      })
    ).toBe(true);
    expect(
      canSubmitChatComposer({
        text: "",
        hasAttachments: false,
        hasQueuedMessages: true,
        hasActiveRun: true,
        isProcessingDocument: false,
        isStopping: false
      })
    ).toBe(false);
    expect(chatComposerShowsStop(true, false)).toBe(true);
    expect(chatComposerShowsStop(false, true)).toBe(true);
  });

  test("processing and stopping are hard submission fences", () => {
    expect(
      canSubmitChatComposer({
        text: "message",
        hasAttachments: false,
        hasQueuedMessages: false,
        hasActiveRun: false,
        isProcessingDocument: true,
        isStopping: false
      })
    ).toBe(false);
    expect(
      canSubmitChatComposer({
        text: "message",
        hasAttachments: false,
        hasQueuedMessages: false,
        hasActiveRun: false,
        isProcessingDocument: false,
        isStopping: true
      })
    ).toBe(false);
  });

  test("a blank text-only edit is disabled while an attachment-backed edit can submit", () => {
    expect(
      canSubmitChatComposer({
        text: "",
        hasAttachments: false,
        hasQueuedMessages: true,
        isEditingQueuedMessage: true,
        hasActiveRun: false,
        isProcessingDocument: false,
        isStopping: false
      })
    ).toBe(false);
    expect(
      canSubmitChatComposer({
        text: "",
        hasAttachments: true,
        hasQueuedMessages: true,
        isEditingQueuedMessage: true,
        hasActiveRun: false,
        isProcessingDocument: false,
        isStopping: false
      })
    ).toBe(true);
  });

  test("does not invent queue state while constructing a fixture", () => {
    expect(emptyChatComposerQueueState()).toEqual({ items: [], edit: null });
  });
});
