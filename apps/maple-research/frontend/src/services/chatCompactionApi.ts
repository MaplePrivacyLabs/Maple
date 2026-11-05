import type OpenAI from "openai";
import { parseCompactionPage } from "./chatCompaction";

/**
 * Additive OpenSecret metadata, over the same account-bound encrypted fetch as
 * chat. Use that transport's configured timeout, not a short metadata deadline
 * that could abort a healthy read and retire the shared encrypted session.
 */
export async function readChatCompactions(
  client: OpenAI,
  conversationId: string,
  after?: string,
  signal?: AbortSignal
) {
  const result = await client.get<unknown>(
    `/conversations/${encodeURIComponent(conversationId)}/compactions`,
    { query: { limit: 100, order: "desc", ...(after ? { after } : {}) }, signal }
  );
  return parseCompactionPage(result);
}
