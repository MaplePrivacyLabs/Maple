import {
  chatQueuedMessageHasContent,
  detachChatComposerDraft,
  stageChatComposerDraft,
  takeNextChatQueuedMessage,
  updateChatQueuedMessage,
  type ChatComposerDraft,
  type ChatAccountQueueUsage,
  type ChatQueueAdmissionFailureStatus,
  type ChatQueuedMessage,
  type ChatQueuedMessageMetadata,
  MAX_CHAT_ACCOUNT_RETAINED_QUEUE_MESSAGES
} from "./chatComposerQueue";

export type ChatComposerSubmissionPlan<TComposer extends ChatComposerDraft> =
  | Readonly<{
      status: ChatQueueAdmissionFailureStatus | "processing" | "missing_edit";
    }>
  | Readonly<{ status: "queued" | "updated"; composer: TComposer }>
  | Readonly<{
      status: "start";
      composer: TComposer;
      item: ChatQueuedMessage;
      recoverOnFailure: boolean;
    }>;

/**
 * Applies an externally produced composer value, such as a completed voice
 * transcription, without disturbing the draft's queue or attachment state.
 * Callers can also use this before a send fence returns so the produced text
 * remains editable instead of being dropped.
 */
export function chatComposerWithInputOverride<TComposer extends ChatComposerDraft>(
  composer: TComposer,
  overrideInput: string | undefined
): TComposer {
  if (overrideInput === undefined || overrideInput === composer.input) return composer;
  return { ...composer, input: overrideInput };
}

/**
 * Plans a submit as one synchronous composer mutation. In particular, a live
 * draft is detached before image conversion or any network work can yield, so
 * typing the next message can never be cleared by the previous send.
 */
export function planChatComposerSubmission<TComposer extends ChatComposerDraft>({
  composer,
  hasActiveRun,
  metadata,
  accountUsage
}: {
  composer: TComposer;
  hasActiveRun: boolean;
  metadata: ChatQueuedMessageMetadata;
  accountUsage?: ChatAccountQueueUsage;
}): ChatComposerSubmissionPlan<TComposer> {
  if (composer.isProcessingDocument) return { status: "processing" };

  if (composer.queue.edit) {
    const updated = updateChatQueuedMessage(
      composer.queue,
      composer.queue.edit.queueId,
      composer.input
    );
    if (updated.status === "text_too_large") return { status: "text_too_large" };
    if (updated.status === "empty") return { status: "empty" };
    if (updated.status === "missing") return { status: "missing_edit" };
    if (updated.status !== "updated") return { status: "missing_edit" };

    const updatedComposer = {
      ...composer,
      input: updated.restoreInput ?? composer.input,
      queue: updated.queue
    };
    if (hasActiveRun) return { status: "updated", composer: updatedComposer };

    const next = takeNextChatQueuedMessage(updatedComposer.queue);
    if (next.status !== "taken") return { status: "missing_edit" };
    return {
      status: "start",
      composer: { ...updatedComposer, queue: next.queue },
      item: next.item,
      recoverOnFailure: false
    };
  }

  const detached = detachChatComposerDraft(composer, metadata);
  const liveDraftHasContent = chatQueuedMessageHasContent(detached.item);

  if (hasActiveRun) {
    if (!liveDraftHasContent) return { status: "empty" };
    const staged = stageChatComposerDraft(composer, metadata, accountUsage);
    if (staged.status !== "enqueued") return { status: staged.status };
    return { status: "queued", composer: staged.composer };
  }

  if (composer.queue.items.length === 0) {
    if (!liveDraftHasContent) return { status: "empty" };
    // Reserve one account-wide queue slot while this detached turn is in
    // flight. If it fails before the request becomes authoritative, recovery
    // may need to put it ahead of a draft the user typed meanwhile.
    if (
      accountUsage &&
      accountUsage.queuedMessageCount >= MAX_CHAT_ACCOUNT_RETAINED_QUEUE_MESSAGES
    ) {
      return { status: "account_queue_full" };
    }
    return {
      status: "start",
      composer: detached.composer,
      item: detached.item,
      recoverOnFailure: true
    };
  }

  const next = takeNextChatQueuedMessage(composer.queue);
  if (next.status !== "taken") return { status: "empty" };

  let preparedComposer = { ...composer, queue: next.queue };
  if (liveDraftHasContent) {
    // Free the oldest slot before appending the live draft. This keeps a full
    // queue resumable without ever retaining more than the configured limit.
    const staged = stageChatComposerDraft(preparedComposer, metadata, accountUsage);
    if (
      staged.status === "queue_full" ||
      staged.status === "queue_payload_too_large" ||
      staged.status === "account_queue_full" ||
      staged.status === "account_payload_too_large"
    ) {
      // The oldest item is already retained by the active turn. Keep the live
      // draft editable while an over-cap lossless recovery/rekey FIFO drains,
      // or until the account-wide reservation is released.
    } else if (staged.status !== "enqueued") {
      return { status: staged.status };
    } else {
      preparedComposer = staged.composer;
    }
  }

  return {
    status: "start",
    composer: preparedComposer,
    item: next.item,
    recoverOnFailure: false
  };
}

export function canSubmitChatComposer({
  text,
  hasAttachments,
  hasQueuedMessages,
  isEditingQueuedMessage = false,
  hasActiveRun,
  isProcessingDocument,
  isStopping
}: {
  text: string;
  hasAttachments: boolean;
  hasQueuedMessages: boolean;
  isEditingQueuedMessage?: boolean;
  hasActiveRun: boolean;
  isProcessingDocument: boolean;
  isStopping: boolean;
}): boolean {
  if (isProcessingDocument || isStopping) return false;
  const hasLiveContent = Boolean(text.trim()) || hasAttachments;
  if (isEditingQueuedMessage) return hasLiveContent;
  return hasLiveContent || (hasQueuedMessages && !hasActiveRun);
}

export function chatComposerShowsStop(isGenerating: boolean, isStopping: boolean): boolean {
  return isGenerating || isStopping;
}
