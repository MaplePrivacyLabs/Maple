const unresolvedResponseMessagesByStore = new WeakMap<object, Map<number, string>>();

function messagesFor(store: object): Map<number, string> {
  const existing = unresolvedResponseMessagesByStore.get(store);
  if (existing) return existing;
  const created = new Map<number, string>();
  unresolvedResponseMessagesByStore.set(store, created);
  return created;
}

/**
 * Records the one optimistic user-message UUID owned by an active run before
 * its Responses POST is dispatched. Polling must use this exact UUID rather
 * than adopting any older incomplete transcript row.
 */
export function registerUnresolvedChatResponseMessage(
  store: object,
  runToken: number,
  messageId: string
): void {
  messagesFor(store).set(runToken, messageId);
}

export function getUnresolvedChatResponseMessage(
  store: object,
  runToken: number
): string | undefined {
  return unresolvedResponseMessagesByStore.get(store)?.get(runToken);
}

export function clearUnresolvedChatResponseMessage(
  store: object,
  runToken: number,
  expectedMessageId?: string
): void {
  const messages = unresolvedResponseMessagesByStore.get(store);
  if (!messages) return;
  if (expectedMessageId !== undefined && messages.get(runToken) !== expectedMessageId) return;
  messages.delete(runToken);
  if (messages.size === 0) unresolvedResponseMessagesByStore.delete(store);
}
