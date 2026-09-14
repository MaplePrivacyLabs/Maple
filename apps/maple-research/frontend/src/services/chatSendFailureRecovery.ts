export type AdoptedDestinationFailureRecovery<TMessage, TComposer> = Readonly<{
  messages: TMessage[];
  composer: TComposer;
}>;

export function chatCursorAfterSendFailure({
  currentCursor,
  optimisticMessageId,
  previousCursor,
  responseCreated
}: {
  currentCursor: string | undefined;
  optimisticMessageId: string;
  previousCursor: string | undefined;
  responseCreated: boolean;
}): string | undefined {
  if (currentCursor !== optimisticMessageId || responseCreated) return currentCursor;
  return previousCursor;
}

/**
 * Once a source run adopts an independently selected destination runtime, that
 * destination owns its composer and object URLs. A later source-send failure
 * must leave those resources untouched and keep the source prompt recoverable
 * in the optimistic message instead of restoring it over the destination draft.
 */
export function recoverFailedSendAfterDestinationAdoption<
  TMessage extends Readonly<{ id: string }>,
  TComposer
>(
  adoptedExistingDestination: boolean,
  messages: readonly TMessage[],
  composer: TComposer,
  sourceMessageId: string
): AdoptedDestinationFailureRecovery<TMessage, TComposer> | null {
  if (!adoptedExistingDestination) return null;

  return {
    messages: messages.map((message) =>
      message.id === sourceMessageId ? ({ ...message, status: "incomplete" } as TMessage) : message
    ),
    composer
  };
}
