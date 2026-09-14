import { describe, expect, test } from "bun:test";
import {
  beginQueuedMessageEdit,
  discardQueuedMessageEdit,
  queuedMessageEditStillPresent,
  type QueuedComposerMessage
} from "./composerQueue";

function queued(queueId: string, text: string): QueuedComposerMessage {
  return { queueId, text };
}

describe("shared composer queue editing", () => {
  test("starts an in-place edit and keeps an unpublished draft stashed", () => {
    const started = beginQueuedMessageEdit({
      current: null,
      scopeKey: "scope-1",
      item: queued("q1", "oldest"),
      composerText: "new draft"
    });

    expect(started).toEqual({
      edit: { scopeKey: "scope-1", queueId: "q1", stashedDraft: "new draft" },
      composer: "oldest"
    });
    expect(discardQueuedMessageEdit(started!.edit)).toBe("new draft");
  });

  test("switching items in one scope keeps the original draft and does not restack", () => {
    const first = beginQueuedMessageEdit({
      current: null,
      scopeKey: "scope-1",
      item: queued("q1", "oldest"),
      composerText: "new draft"
    });
    const second = beginQueuedMessageEdit({
      current: first!.edit,
      scopeKey: "scope-1",
      item: queued("q2", "middle"),
      composerText: "oldest"
    });

    expect(second).toEqual({
      edit: { scopeKey: "scope-1", queueId: "q2", stashedDraft: "new draft" },
      composer: "middle"
    });
    expect(
      beginQueuedMessageEdit({
        current: second!.edit,
        scopeKey: "scope-1",
        item: queued("q2", "middle"),
        composerText: "middle"
      })
    ).toBeNull();
    expect(
      queuedMessageEditStillPresent(second!.edit, [queued("q1", "oldest"), queued("q2", "middle")])
    ).toBe(true);
    expect(queuedMessageEditStillPresent(second!.edit, [queued("q1", "oldest")])).toBe(false);
  });

  test("a different scope stashes that scope's current draft", () => {
    const next = beginQueuedMessageEdit({
      current: {
        scopeKey: "scope-1",
        queueId: "q1",
        stashedDraft: "scope one draft"
      },
      scopeKey: "scope-2",
      item: queued("q1", "scope two queued"),
      composerText: "scope two draft"
    });

    expect(next).toEqual({
      edit: { scopeKey: "scope-2", queueId: "q1", stashedDraft: "scope two draft" },
      composer: "scope two queued"
    });
  });
});
