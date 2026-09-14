import {
  registeredChatTurnCanSettleLocallyForDeletion,
  restoreRegisteredChatTurnBeforeRequest
} from "./chatCurrentTurnRegistry";
import { conversationIdFromChatRuntimeKey } from "./chatRuntimeNavigation";
import {
  classifyChatResponseReconciliation,
  responseIdForChatMessage
} from "./chatResponseReconciliation";
import { isChatResponseCancellationAlreadyTerminalError } from "./chatResponseErrors";
import type { ChatRuntimeKey, ChatRuntimeStore } from "./chatRuntimeStore";
import {
  clearUnresolvedChatResponseMessage,
  getUnresolvedChatResponseMessage
} from "./chatUnresolvedResponseOwnership";

type ActiveChatRuntimeLookup = object & {
  getActiveRunKeys: () => readonly ChatRuntimeKey[];
  resolveKey?: (key: ChatRuntimeKey) => ChatRuntimeKey;
  getActiveRunGroupId?: (key: ChatRuntimeKey) => string | null | undefined;
  get: (
    key: ChatRuntimeKey
  ) => Readonly<{ runToken: number | null; currentResponseId: string | undefined }> | undefined;
  setCurrentResponseId?: (key: ChatRuntimeKey, runToken: number, responseId: string) => boolean;
  cancelRun: (key: ChatRuntimeKey, runToken: number) => unknown;
};

const CHAT_HISTORY_CANCEL_REQUEST_TIMEOUT_MS = 5000;
const CHAT_HISTORY_CANCEL_RETRY_INTERVAL_MS = 250;

type ChatResponseCancellationClient = {
  responses: {
    cancel: (responseId: string, options?: { timeout?: number }) => PromiseLike<unknown>;
    retrieve: (
      responseId: string,
      query?: undefined,
      options?: { timeout?: number }
    ) => PromiseLike<{ status?: string | null }>;
  };
};

type ChatResponseOwnershipClient = {
  conversations: {
    items: {
      retrieve: (
        messageId: string,
        query: { conversation_id: string }
      ) => PromiseLike<{ id?: string; role?: string; response_id?: unknown }>;
    };
  };
};

async function recoverChatResponseOwnershipForHistoryDeletion(
  client: ChatResponseOwnershipClient,
  store: ActiveChatRuntimeLookup,
  key: ChatRuntimeKey,
  runToken: number
): Promise<string | undefined> {
  const messageId = getUnresolvedChatResponseMessage(store, runToken);
  const conversationId = conversationIdFromChatRuntimeKey(store.resolveKey?.(key) ?? key);
  if (!messageId || !conversationId || !store.setCurrentResponseId) return undefined;

  try {
    const item = await client.conversations.items.retrieve(messageId, {
      conversation_id: conversationId
    });
    const responseId = responseIdForChatMessage(messageId, [item]);
    if (!responseId || !store.setCurrentResponseId(key, runToken, responseId)) return undefined;
    clearUnresolvedChatResponseMessage(store, runToken, messageId);
    return responseId;
  } catch (error) {
    if ((error as { status?: unknown })?.status === 404) return undefined;
    throw error;
  }
}

export async function cancelChatResponseForHistoryDeletion(
  client: ChatResponseCancellationClient,
  responseId: string
): Promise<unknown> {
  try {
    return await client.responses.cancel(responseId, {
      timeout: CHAT_HISTORY_CANCEL_REQUEST_TIMEOUT_MS
    });
  } catch (cancellationError) {
    if (!isChatResponseCancellationAlreadyTerminalError(cancellationError)) {
      throw cancellationError;
    }
    // A detached response can finish between discovery and cancellation. Its
    // cancel endpoint's explicit 400 certifies that execution is quiescent even
    // though deletion still needs to confirm the exact durable terminal state.
    try {
      const response = await client.responses.retrieve(responseId, undefined, {
        timeout: CHAT_HISTORY_CANCEL_REQUEST_TIMEOUT_MS
      });
      if (classifyChatResponseReconciliation(response.status) !== "pending") {
        return response;
      }
    } catch {
      // Preserve the cancellation failure so the bounded quiescence loop can
      // retry. A failed status check is never permission to delete.
    }
    throw cancellationError;
  }
}

/** Keep a failed destructive request renderable after its response was already cancelled. */
export function settleChatRuntimeAfterHistoryCancellation<TConversation, TMessage, TComposer>(
  store: ChatRuntimeStore<TConversation, TMessage, TComposer>,
  key: ChatRuntimeKey,
  runToken: number
): boolean {
  const cancelled = store.cancelRun(key, runToken);
  if (!cancelled) return false;
  store.update(key, (snapshot) => ({
    ...snapshot,
    messages: snapshot.messages.map((message) => {
      if (!message || typeof message !== "object" || !("status" in message)) return message;
      const status = (message as { status?: unknown }).status;
      if (status !== "in_progress" && status !== "streaming" && status !== "searching") {
        return message;
      }
      return { ...message, status: "incomplete" } as TMessage;
    })
  }));
  return true;
}

function deletionTimeoutError(): Error {
  return new Error("Timed out waiting for active chat requests to stop");
}

async function settleCancellationRequestsBeforeDeadline(
  requests: readonly Promise<unknown>[],
  deadline: number
): Promise<PromiseSettledResult<unknown>[]> {
  if (requests.length === 0) return [];
  const remainingMs = deadline - Date.now();
  if (remainingMs <= 0) throw deletionTimeoutError();

  let timeout: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      Promise.allSettled(requests),
      new Promise<never>((_, reject) => {
        timeout = setTimeout(() => reject(deletionTimeoutError()), remainingMs);
      })
    ]);
  } finally {
    if (timeout !== undefined) clearTimeout(timeout);
  }
}

export async function quiesceChatRuntimeRunsForHistoryDeletion({
  store,
  cancelResponse,
  responseOwnershipClient,
  settleCancelledRun,
  keys,
  activityGroupId,
  timeoutMs = 30_000
}: {
  store: ActiveChatRuntimeLookup;
  cancelResponse: (responseId: string) => Promise<unknown>;
  responseOwnershipClient?: ChatResponseOwnershipClient;
  settleCancelledRun?: (key: ChatRuntimeKey, runToken: number) => unknown;
  keys?: readonly ChatRuntimeKey[];
  activityGroupId?: string;
  timeoutMs?: number;
}): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  const cancellationAccepted = new Set<string>();
  const cancellationLastAttemptMs = new Map<string, number>();
  const ownershipLastAttemptMs = new Map<string, number>();
  const resolveKey = (key: ChatRuntimeKey) => store.resolveKey?.(key) ?? key;
  const targetActiveRunKeys = () => {
    const activeKeys = store.getActiveRunKeys();
    if (!keys && !activityGroupId) return activeKeys;
    return activeKeys.filter(
      (activeKey) =>
        keys?.some((key) => resolveKey(key) === resolveKey(activeKey)) ||
        (activityGroupId !== undefined &&
          store.getActiveRunGroupId?.(activeKey) === activityGroupId)
    );
  };

  while (targetActiveRunKeys().length > 0) {
    const responseIds: string[] = [];
    const ownershipRequests: Promise<unknown>[] = [];
    const now = Date.now();
    for (const key of targetActiveRunKeys()) {
      const snapshot = store.get(key);
      if (!snapshot || snapshot.runToken === null) continue;
      const canSettleLocally = registeredChatTurnCanSettleLocallyForDeletion(
        store,
        snapshot.runToken
      );
      const restoredLocally =
        canSettleLocally &&
        restoreRegisteredChatTurnBeforeRequest(
          store,
          snapshot.runToken,
          "Sending paused while chat history is being deleted."
        );
      if (restoredLocally) {
        store.cancelRun(key, snapshot.runToken);
        clearUnresolvedChatResponseMessage(store, snapshot.runToken);
        continue;
      }
      if (!snapshot.currentResponseId && responseOwnershipClient) {
        const ownershipKey = `${resolveKey(key)}:${snapshot.runToken}`;
        const lastOwnershipAttempt = ownershipLastAttemptMs.get(ownershipKey);
        if (
          lastOwnershipAttempt === undefined ||
          now - lastOwnershipAttempt >= CHAT_HISTORY_CANCEL_RETRY_INTERVAL_MS
        ) {
          ownershipLastAttemptMs.set(ownershipKey, now);
          ownershipRequests.push(
            recoverChatResponseOwnershipForHistoryDeletion(
              responseOwnershipClient,
              store,
              key,
              snapshot.runToken
            )
          );
        }
      }
      const lastAttempt = snapshot.currentResponseId
        ? cancellationLastAttemptMs.get(snapshot.currentResponseId)
        : undefined;
      if (
        snapshot.currentResponseId &&
        !cancellationAccepted.has(snapshot.currentResponseId) &&
        (lastAttempt === undefined || now - lastAttempt >= CHAT_HISTORY_CANCEL_RETRY_INTERVAL_MS)
      ) {
        cancellationLastAttemptMs.set(snapshot.currentResponseId, now);
        responseIds.push(snapshot.currentResponseId);
      }
    }

    await settleCancellationRequestsBeforeDeadline(ownershipRequests, deadline);

    // Cancellation can lose a race with natural completion: the endpoint then
    // rejects because the response is already terminal even though its SSE
    // terminal frame is about to settle the local run. Treat cancellation as a
    // request, then keep the deletion fence closed until every run actually
    // settles (or the bounded timeout fails closed).
    const cancellationResults = await settleCancellationRequestsBeforeDeadline(
      responseIds.map(cancelResponse),
      deadline
    );
    cancellationResults.forEach((result, index) => {
      if (result.status !== "fulfilled") return;
      const responseId = responseIds[index];
      cancellationAccepted.add(responseId);
      // The cancellation endpoint commits terminal response ownership before
      // it resolves. An offscreen detached run has no SSE listener left to
      // settle its client token, so retire that exact response locally here.
      for (const key of targetActiveRunKeys()) {
        const snapshot = store.get(key);
        if (
          snapshot?.runToken !== null &&
          snapshot?.runToken !== undefined &&
          snapshot.currentResponseId === responseId
        ) {
          if (settleCancelledRun) settleCancelledRun(key, snapshot.runToken);
          else store.cancelRun(key, snapshot.runToken);
        }
      }
    });
    if (targetActiveRunKeys().length === 0) return;
    if (Date.now() >= deadline) {
      throw deletionTimeoutError();
    }
    // A conversation-create request has no response ID yet. Let it settle and
    // surface its ID (if any), then cancel it before issuing delete-all.
    await new Promise((resolve) => setTimeout(resolve, Math.min(25, deadline - Date.now())));
  }
}
