import {
  beginQueuedMessageEdit as beginSharedQueuedMessageEdit,
  discardQueuedMessageEdit as discardSharedQueuedMessageEdit,
  queuedMessageEditStillPresent as sharedQueuedMessageEditStillPresent
} from "./composerQueue";

export interface AgentQueuedMessage {
  queueId: string;
  messageId: string;
  sessionId: string;
  text: string;
  attachments?: Array<{ id: string; name: string; mimeType: string; source: string }>;
  createdMs: number;
}

export interface AgentDesktopQueueSnapshot {
  revision: number;
  items: AgentQueuedMessage[];
}

export function emptyAgentDesktopQueueSnapshot(): AgentDesktopQueueSnapshot {
  return { revision: 0, items: [] };
}

export function applyAgentDesktopQueueSnapshot(
  current: AgentDesktopQueueSnapshot | undefined,
  incoming: AgentDesktopQueueSnapshot
): AgentDesktopQueueSnapshot {
  if (current && incoming.revision < current.revision) {
    return current;
  }
  return incoming;
}

export function queueSnapshotWithoutItem(
  current: AgentDesktopQueueSnapshot | undefined,
  queueId: string
): AgentDesktopQueueSnapshot {
  const base = current ?? emptyAgentDesktopQueueSnapshot();
  return {
    revision: base.revision,
    items: base.items.filter((item) => item.queueId !== queueId)
  };
}

export interface AgentQueuedMessageEdit {
  sessionId: string;
  queueId: string;
  stashedDraft: string;
}

export function restoreQueuedMessageToComposer(currentInput: string, queuedText: string): string {
  const draft = currentInput.trim();
  return draft ? `${queuedText}\n${currentInput}` : queuedText;
}

export function beginQueuedMessageEdit({
  current,
  sessionId,
  item,
  composerText
}: {
  current: AgentQueuedMessageEdit | null;
  sessionId: string;
  item: AgentQueuedMessage;
  composerText: string;
}): { edit: AgentQueuedMessageEdit; composer: string } | null {
  const result = beginSharedQueuedMessageEdit({
    current: current
      ? {
          scopeKey: current.sessionId,
          queueId: current.queueId,
          stashedDraft: current.stashedDraft
        }
      : null,
    scopeKey: sessionId,
    item,
    composerText
  });
  if (!result) return null;
  return {
    edit: {
      sessionId: result.edit.scopeKey,
      queueId: result.edit.queueId,
      stashedDraft: result.edit.stashedDraft
    },
    composer: result.composer
  };
}

export function discardQueuedMessageEdit(edit: AgentQueuedMessageEdit): string {
  return discardSharedQueuedMessageEdit({
    scopeKey: edit.sessionId,
    queueId: edit.queueId,
    stashedDraft: edit.stashedDraft
  });
}

export function queuedMessageEditStillPresent(
  edit: AgentQueuedMessageEdit | null,
  items: AgentQueuedMessage[]
): boolean {
  return sharedQueuedMessageEditStillPresent(
    edit
      ? {
          scopeKey: edit.sessionId,
          queueId: edit.queueId,
          stashedDraft: edit.stashedDraft
        }
      : null,
    items
  );
}

export function shouldPrepareThoughtAfterAgentSend(queued?: AgentQueuedMessage | null): boolean {
  return !queued;
}
