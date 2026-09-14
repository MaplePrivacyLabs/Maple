export type NormalizedChatPollingPage<T> = Readonly<{
  chronologicalItems: T[];
  newestCompletedItem: T | undefined;
}>;

/**
 * Cursor polls arrive ascending. A recovery poll without a durable cursor asks
 * for the newest page descending, then reverses it before transcript merging.
 */
export function normalizeChatPollingPage<T extends { id?: string; status?: string | null }>(
  items: readonly T[],
  hasCursor: boolean
): NormalizedChatPollingPage<T> {
  const newestFirst = hasCursor ? [...items].reverse() : [...items];
  return {
    chronologicalItems: hasCursor ? [...items] : [...items].reverse(),
    newestCompletedItem: newestFirst.find((item) => item.status !== "in_progress")
  };
}
