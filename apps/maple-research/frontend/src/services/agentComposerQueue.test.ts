import { describe, expect, test } from "bun:test";
import {
  applyAgentDesktopQueueSnapshot,
  beginQueuedMessageEdit,
  discardQueuedMessageEdit,
  emptyAgentDesktopQueueSnapshot,
  queuedMessageEditStillPresent,
  queueSnapshotWithoutItem,
  restoreQueuedMessageToComposer,
  shouldPrepareThoughtAfterAgentSend,
  type AgentQueuedMessage
} from "./agentComposerQueue";

function queued(queueId: string, text: string): AgentQueuedMessage {
  return {
    queueId,
    messageId: `msg-${queueId}`,
    sessionId: "session-1",
    text,
    createdMs: 1
  };
}

describe("agent composer queue projection", () => {
  test("ignores stale snapshots and accepts equal or newer revisions", () => {
    const first = { revision: 2, items: [queued("q1", "later")] };
    expect(
      applyAgentDesktopQueueSnapshot(first, { revision: 1, items: [queued("q0", "stale")] })
    ).toBe(first);
    expect(
      applyAgentDesktopQueueSnapshot(first, { revision: 2, items: [queued("q1", "same")] }).items[0]
        ?.text
    ).toBe("same");
    expect(
      applyAgentDesktopQueueSnapshot(undefined, emptyAgentDesktopQueueSnapshot()).items
    ).toEqual([]);
  });

  test("optimistically drops a chip without advancing the native revision", () => {
    const snapshot = {
      revision: 4,
      items: [queued("q1", "keep"), queued("q2", "drop")]
    };
    expect(queueSnapshotWithoutItem(snapshot, "q2")).toEqual({
      revision: 4,
      items: [queued("q1", "keep")]
    });
    expect(queueSnapshotWithoutItem(undefined, "missing")).toEqual(
      emptyAgentDesktopQueueSnapshot()
    );
  });

  test("restores an unqueued message without discarding a composer draft", () => {
    expect(restoreQueuedMessageToComposer("", "queued")).toBe("queued");
    expect(restoreQueuedMessageToComposer("   ", "queued")).toBe("queued");
    expect(restoreQueuedMessageToComposer("draft", "queued")).toBe("queued\ndraft");
  });

  test("pencil starts an in-place edit and keeps an unpublished draft stashed", () => {
    const started = beginQueuedMessageEdit({
      current: null,
      sessionId: "session-1",
      item: queued("q1", "oldest"),
      composerText: "new draft"
    });
    expect(started).toEqual({
      edit: { sessionId: "session-1", queueId: "q1", stashedDraft: "new draft" },
      composer: "oldest"
    });
    expect(discardQueuedMessageEdit(started!.edit)).toBe("new draft");
  });

  test("switching chips keeps the original draft and does not restack", () => {
    const first = beginQueuedMessageEdit({
      current: null,
      sessionId: "session-1",
      item: queued("q1", "oldest"),
      composerText: "new draft"
    });
    const second = beginQueuedMessageEdit({
      current: first!.edit,
      sessionId: "session-1",
      item: queued("q2", "middle"),
      composerText: "oldest"
    });
    expect(second).toEqual({
      edit: { sessionId: "session-1", queueId: "q2", stashedDraft: "new draft" },
      composer: "middle"
    });
    expect(
      beginQueuedMessageEdit({
        current: second!.edit,
        sessionId: "session-1",
        item: queued("q2", "middle"),
        composerText: "middle"
      })
    ).toBeNull();
    expect(
      queuedMessageEditStillPresent(second!.edit, [queued("q1", "oldest"), queued("q2", "middle")])
    ).toBe(true);
    expect(queuedMessageEditStillPresent(second!.edit, [queued("q1", "oldest")])).toBe(false);
  });

  test("preserves the Agent session-keyed edit API over the shared helper", () => {
    const next = beginQueuedMessageEdit({
      current: {
        sessionId: "session-1",
        queueId: "q1",
        stashedDraft: "session one draft"
      },
      sessionId: "session-2",
      item: queued("q2", "session two queued"),
      composerText: "session two draft"
    });

    expect(next).toEqual({
      edit: {
        sessionId: "session-2",
        queueId: "q2",
        stashedDraft: "session two draft"
      },
      composer: "session two queued"
    });
  });

  test("does not seed thought tracking for a staged follow-up", () => {
    expect(shouldPrepareThoughtAfterAgentSend(undefined)).toBe(true);
    expect(shouldPrepareThoughtAfterAgentSend(null)).toBe(true);
    expect(shouldPrepareThoughtAfterAgentSend(queued("q1", "later"))).toBe(false);
  });
});
