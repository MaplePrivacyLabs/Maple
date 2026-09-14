export type ChatResponseReconciliation = "completed" | "terminal" | "pending";

type ChatResponseLinkedConversationItem = Readonly<{
  id?: string;
  role?: string;
  response_id?: unknown;
}>;

export function classifyChatResponseReconciliation(
  status: string | null | undefined
): ChatResponseReconciliation {
  if (status === "completed") return "completed";
  if (status === "failed" || status === "cancelled" || status === "incomplete") {
    return "terminal";
  }
  return "pending";
}

/**
 * Recovers the response that owns one exact optimistic user item when the
 * streaming request was accepted but its `response.created` frame never
 * reached Maple.
 */
export function responseIdForChatMessage(
  messageId: string,
  polledItems: readonly ChatResponseLinkedConversationItem[]
): string | undefined {
  for (const item of polledItems) {
    if (item.id !== messageId || item.role !== "user") continue;
    if (typeof item.response_id === "string" && item.response_id.length > 0) {
      return item.response_id;
    }
  }

  return undefined;
}
