const haltedRunTokensByStore = new WeakMap<object, Set<number>>();

/**
 * Sticky Stop intent for one outer Chat FIFO runner. This is deliberately
 * separate from the transient cancellation-in-flight UI state: a failed remote
 * cancellation may be retried, but the current runner must still never promote
 * another queued turn after the user pressed Stop.
 */
export function requestChatRunQueueHalt(store: object, runToken: number): void {
  const halted = haltedRunTokensByStore.get(store) ?? new Set<number>();
  halted.add(runToken);
  haltedRunTokensByStore.set(store, halted);
}

export function isChatRunQueueHaltRequested(store: object, runToken: number): boolean {
  return haltedRunTokensByStore.get(store)?.has(runToken) ?? false;
}

export function clearChatRunQueueHalt(store: object, runToken: number): void {
  const halted = haltedRunTokensByStore.get(store);
  if (!halted) return;
  halted.delete(runToken);
  if (halted.size === 0) haltedRunTokensByStore.delete(store);
}
