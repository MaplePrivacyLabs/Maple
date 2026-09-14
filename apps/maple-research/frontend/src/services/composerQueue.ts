export interface QueuedComposerMessage {
  queueId: string;
  text: string;
}

export interface QueuedMessageEdit {
  scopeKey: string;
  queueId: string;
  stashedDraft: string;
}

export function beginQueuedMessageEdit({
  current,
  scopeKey,
  item,
  composerText
}: {
  current: QueuedMessageEdit | null;
  scopeKey: string;
  item: QueuedComposerMessage;
  composerText: string;
}): { edit: QueuedMessageEdit; composer: string } | null {
  if (current?.scopeKey === scopeKey && current.queueId === item.queueId) {
    return null;
  }
  return {
    edit: {
      scopeKey,
      queueId: item.queueId,
      stashedDraft: current?.scopeKey === scopeKey ? current.stashedDraft : composerText
    },
    composer: item.text
  };
}

export function discardQueuedMessageEdit(edit: QueuedMessageEdit): string {
  return edit.stashedDraft;
}

export function queuedMessageEditStillPresent(
  edit: QueuedMessageEdit | null,
  items: readonly Pick<QueuedComposerMessage, "queueId">[]
): boolean {
  return Boolean(edit && items.some((item) => item.queueId === edit.queueId));
}
