import {
  beginQueuedMessageEdit as beginSharedQueuedMessageEdit,
  discardQueuedMessageEdit as discardSharedQueuedMessageEdit,
  queuedMessageEditStillPresent as sharedQueuedMessageEditStillPresent,
  type QueuedMessageEdit
} from "./composerQueue";

export const MAX_CHAT_QUEUED_MESSAGES = 16;
export const MAX_CHAT_QUEUED_TEXT_BYTES = 32 * 1024;
export const MAX_CHAT_QUEUED_IMAGES_PER_ITEM = 10;
export const MAX_CHAT_QUEUED_IMAGE_BYTES = 20 * 1024 * 1024;
export const MAX_CHAT_QUEUED_DOCUMENT_BYTES = 10 * 1024 * 1024;
export const MAX_CHAT_QUEUED_AGGREGATE_ATTACHMENT_BYTES = 256 * 1024 * 1024;
export const MAX_CHAT_ACCOUNT_RETAINED_QUEUE_MESSAGES = 64;
export const MAX_CHAT_ACCOUNT_RETAINED_IMAGES =
  MAX_CHAT_ACCOUNT_RETAINED_QUEUE_MESSAGES * MAX_CHAT_QUEUED_IMAGES_PER_ITEM;
export const MAX_CHAT_ACCOUNT_RETAINED_ATTACHMENT_BYTES = 256 * 1024 * 1024;

export type ChatQueuedMessage = {
  queueId: string;
  messageId: string;
  text: string;
  draftImages: File[];
  imageUrls: Map<File, string>;
  documentText: string;
  documentName: string;
  draftProjectId: string | null;
  model: string;
  webSearchEnabled: boolean;
  createdMs: number;
};

export type ChatQueuedMessageEdit = QueuedMessageEdit;

export type ChatComposerQueueState = {
  items: ChatQueuedMessage[];
  edit: ChatQueuedMessageEdit | null;
};

export type ChatQueuedMessageMetadata = Pick<
  ChatQueuedMessage,
  "queueId" | "messageId" | "model" | "webSearchEnabled" | "createdMs"
>;

export type ChatComposerDraft = {
  input: string;
  draftImages: File[];
  imageUrls: Map<File, string>;
  documentText: string;
  documentName: string;
  draftProjectId: string | null;
  isProcessingDocument: boolean;
  imagePasteGeneration: number;
  documentUploadGeneration: number;
  queue: ChatComposerQueueState;
};

export type ChatQueueAdmissionFailureStatus =
  | "empty"
  | "queue_full"
  | "text_too_large"
  | "too_many_images"
  | "image_too_large"
  | "document_too_large"
  | "queue_payload_too_large"
  | "account_queue_full"
  | "account_payload_too_large";

export type ChatAccountQueueUsage = Readonly<{
  queuedMessageCount: number;
  attachmentBytes: number;
  imageCount: number;
}>;

export type EnqueueChatQueuedMessageResult =
  | Readonly<{ status: "enqueued"; queue: ChatComposerQueueState }>
  | Readonly<{
      status: ChatQueueAdmissionFailureStatus;
      queue: ChatComposerQueueState;
    }>;

export type CancelChatQueuedMessageResult =
  | Readonly<{
      status: "cancelled";
      queue: ChatComposerQueueState;
      item: ChatQueuedMessage;
      restoreInput: string | undefined;
    }>
  | Readonly<{ status: "missing"; queue: ChatComposerQueueState }>;

export type BeginChatQueuedMessageEditResult =
  | Readonly<{
      status: "started";
      queue: ChatComposerQueueState;
      input: string;
    }>
  | Readonly<{
      status: "already_editing" | "missing";
      queue: ChatComposerQueueState;
    }>;

export type EndChatQueuedMessageEditResult =
  | Readonly<{
      status: "ended";
      queue: ChatComposerQueueState;
      restoreInput: string;
    }>
  | Readonly<{ status: "not_editing"; queue: ChatComposerQueueState }>;

export type UpdateChatQueuedMessageResult =
  | Readonly<{
      status: "updated";
      queue: ChatComposerQueueState;
      item: ChatQueuedMessage;
      restoreInput: string | undefined;
    }>
  | Readonly<{ status: "empty"; queue: ChatComposerQueueState }>
  | Readonly<{ status: "missing"; queue: ChatComposerQueueState }>
  | Readonly<{ status: "text_too_large"; queue: ChatComposerQueueState }>;

export type TakeNextChatQueuedMessageResult =
  | Readonly<{
      status: "taken";
      queue: ChatComposerQueueState;
      item: ChatQueuedMessage;
    }>
  | Readonly<{ status: "blocked_by_edit" | "empty"; queue: ChatComposerQueueState }>;

export type StageChatComposerDraftResult<TComposer extends ChatComposerDraft> =
  | Readonly<{
      status: "enqueued";
      composer: TComposer;
      item: ChatQueuedMessage;
    }>
  | Readonly<{
      status: ChatQueueAdmissionFailureStatus;
      composer: TComposer;
    }>;

export type RecoverDetachedChatComposerDraftResult<TComposer extends ChatComposerDraft> = Readonly<{
  status: "restored" | "requeued" | "already_queued";
  composer: TComposer;
}>;

export type MergeChatComposerDraftsResult<TComposer extends ChatComposerDraft> = Readonly<{
  composer: TComposer;
  displacedObjectUrls: string[];
}>;

export function emptyChatComposerQueueState(): ChatComposerQueueState {
  return { items: [], edit: null };
}

export function chatQueuedTextByteLength(text: string): number {
  return new TextEncoder().encode(text).byteLength;
}

export function chatQueuedMessageHasContent(item: ChatQueuedMessage): boolean {
  return Boolean(item.text.trim() || item.draftImages.length || item.documentText);
}

export function chatComposerHasDraftMaterial(composer: ChatComposerDraft): boolean {
  return Boolean(
    composer.input.length ||
    composer.draftImages.length ||
    composer.documentText.length ||
    composer.documentName.length ||
    composer.isProcessingDocument
  );
}

function queuedMessageImages(item: ChatQueuedMessage): File[] {
  const images = new Set(item.draftImages);
  for (const image of item.imageUrls.keys()) images.add(image);
  return Array.from(images);
}

export function chatQueuedMessageAttachmentByteLength(item: ChatQueuedMessage): number {
  let bytes = chatQueuedTextByteLength(item.documentText);
  for (const image of queuedMessageImages(item)) bytes += image.size;
  return bytes;
}

function queuedAttachmentByteLength(items: readonly ChatQueuedMessage[]): number {
  let bytes = 0;
  for (const item of items) bytes += chatQueuedMessageAttachmentByteLength(item);
  return bytes;
}

function validateQueuedMessagePayload(
  queue: ChatComposerQueueState,
  item: ChatQueuedMessage,
  accountUsage?: ChatAccountQueueUsage
): ChatQueueAdmissionFailureStatus | null {
  if (!chatQueuedMessageHasContent(item)) return "empty";
  if (chatQueuedTextByteLength(item.text) > MAX_CHAT_QUEUED_TEXT_BYTES) {
    return "text_too_large";
  }
  const images = queuedMessageImages(item);
  if (images.length > MAX_CHAT_QUEUED_IMAGES_PER_ITEM) {
    return "too_many_images";
  }
  if (images.some((image) => image.size > MAX_CHAT_QUEUED_IMAGE_BYTES)) {
    return "image_too_large";
  }
  if (chatQueuedTextByteLength(item.documentText) > MAX_CHAT_QUEUED_DOCUMENT_BYTES) {
    return "document_too_large";
  }
  if (queue.items.length >= MAX_CHAT_QUEUED_MESSAGES) return "queue_full";
  if (
    queuedAttachmentByteLength(queue.items) + chatQueuedMessageAttachmentByteLength(item) >
    MAX_CHAT_QUEUED_AGGREGATE_ATTACHMENT_BYTES
  ) {
    return "queue_payload_too_large";
  }
  if (accountUsage && accountUsage.queuedMessageCount >= MAX_CHAT_ACCOUNT_RETAINED_QUEUE_MESSAGES) {
    return "account_queue_full";
  }
  if (accountUsage && accountUsage.attachmentBytes > MAX_CHAT_ACCOUNT_RETAINED_ATTACHMENT_BYTES) {
    return "account_payload_too_large";
  }
  return null;
}

export function enqueueChatQueuedMessage(
  queue: ChatComposerQueueState,
  item: ChatQueuedMessage,
  accountUsage?: ChatAccountQueueUsage
): EnqueueChatQueuedMessageResult {
  const rejected = validateQueuedMessagePayload(queue, item, accountUsage);
  if (rejected) return { status: rejected, queue };

  return {
    status: "enqueued",
    queue: { ...queue, items: [...queue.items, item] }
  };
}

export function cancelChatQueuedMessage(
  queue: ChatComposerQueueState,
  queueId: string
): CancelChatQueuedMessageResult {
  const index = queue.items.findIndex((item) => item.queueId === queueId);
  if (index < 0) return { status: "missing", queue };

  const item = queue.items[index];
  const editingThisItem = queue.edit?.queueId === queueId;
  const items = [...queue.items];
  items.splice(index, 1);
  return {
    status: "cancelled",
    queue: {
      items,
      edit: editingThisItem ? null : queue.edit
    },
    item,
    restoreInput:
      editingThisItem && queue.edit ? discardSharedQueuedMessageEdit(queue.edit) : undefined
  };
}

export function beginChatQueuedMessageEdit(
  queue: ChatComposerQueueState,
  scopeKey: string,
  queueId: string,
  composerInput: string
): BeginChatQueuedMessageEditResult {
  const item = queue.items.find((queued) => queued.queueId === queueId);
  if (!item) return { status: "missing", queue };
  // Queue state is runtime-local, so a matching queue ID is the same edit even
  // if draft-to-conversation rekeying changed the runtime scope. Normalize an
  // older real runtime key before switching items so the shared stash logic
  // keeps the pre-edit draft instead of treating it as a different runtime.
  if (queue.edit?.queueId === queueId) return { status: "already_editing", queue };
  const currentEdit = queue.edit ? { ...queue.edit, scopeKey } : null;
  const result = beginSharedQueuedMessageEdit({
    current: currentEdit,
    scopeKey,
    item,
    composerText: composerInput
  });
  if (!result) return { status: "already_editing", queue };

  return {
    status: "started",
    queue: { ...queue, edit: result.edit },
    input: result.composer
  };
}

export function discardChatQueuedMessageEdit(
  queue: ChatComposerQueueState
): EndChatQueuedMessageEditResult {
  if (!queue.edit) return { status: "not_editing", queue };
  return {
    status: "ended",
    queue: { ...queue, edit: null },
    restoreInput: discardSharedQueuedMessageEdit(queue.edit)
  };
}

export function updateChatQueuedMessage(
  queue: ChatComposerQueueState,
  queueId: string,
  text: string
): UpdateChatQueuedMessageResult {
  const index = queue.items.findIndex((item) => item.queueId === queueId);
  if (index < 0) return { status: "missing", queue };

  const trimmedText = text.trim();
  const current = queue.items[index];
  if (!trimmedText && current.draftImages.length === 0 && !current.documentText) {
    return { status: "empty", queue };
  }
  if (chatQueuedTextByteLength(trimmedText) > MAX_CHAT_QUEUED_TEXT_BYTES) {
    return { status: "text_too_large", queue };
  }

  const item = { ...current, text: trimmedText };
  const items = [...queue.items];
  items[index] = item;
  const editingThisItem = queue.edit?.queueId === queueId;
  const restoreInput =
    editingThisItem && queue.edit ? discardSharedQueuedMessageEdit(queue.edit) : undefined;
  return {
    status: "updated",
    queue: { items, edit: editingThisItem ? null : queue.edit },
    item,
    restoreInput
  };
}

export function queuedChatMessageEditStillPresent(queue: ChatComposerQueueState): boolean {
  return sharedQueuedMessageEditStillPresent(queue.edit, queue.items);
}

export function takeNextChatQueuedMessage(
  queue: ChatComposerQueueState
): TakeNextChatQueuedMessageResult {
  if (queue.edit) return { status: "blocked_by_edit", queue };
  const item = queue.items[0];
  if (!item) return { status: "empty", queue };
  return {
    status: "taken",
    queue: { items: queue.items.slice(1), edit: null },
    item
  };
}

export function detachChatComposerDraft<TComposer extends ChatComposerDraft>(
  composer: TComposer,
  metadata: ChatQueuedMessageMetadata
): Readonly<{ composer: TComposer; item: ChatQueuedMessage }> {
  const item: ChatQueuedMessage = {
    ...metadata,
    text: composer.input.trim(),
    draftImages: [...composer.draftImages],
    imageUrls: new Map(composer.imageUrls),
    documentText: composer.documentText,
    documentName: composer.documentName,
    draftProjectId: composer.draftProjectId
  };
  const nextComposer = {
    ...composer,
    input: "",
    draftImages: [],
    imageUrls: new Map<File, string>(),
    documentText: "",
    documentName: "",
    isProcessingDocument: false,
    imagePasteGeneration: composer.imagePasteGeneration + 1,
    documentUploadGeneration: composer.documentUploadGeneration + 1
  };

  return { composer: nextComposer, item };
}

export function stageChatComposerDraft<TComposer extends ChatComposerDraft>(
  composer: TComposer,
  metadata: ChatQueuedMessageMetadata,
  accountUsage?: ChatAccountQueueUsage
): StageChatComposerDraftResult<TComposer> {
  const detached = detachChatComposerDraft(composer, metadata);
  const enqueued = enqueueChatQueuedMessage(composer.queue, detached.item, accountUsage);
  if (enqueued.status !== "enqueued") {
    return { status: enqueued.status, composer };
  }

  return {
    status: "enqueued",
    composer: { ...detached.composer, queue: enqueued.queue },
    item: detached.item
  };
}

export function recoverDetachedChatComposerDraft<TComposer extends ChatComposerDraft>(
  composer: TComposer,
  item: ChatQueuedMessage
): RecoverDetachedChatComposerDraftResult<TComposer> {
  if (
    composer.queue.items.some(
      (queued) => queued.queueId === item.queueId || queued.messageId === item.messageId
    )
  ) {
    return { status: "already_queued", composer };
  }

  if (
    !chatComposerHasDraftMaterial(composer) &&
    composer.queue.items.length === 0 &&
    composer.queue.edit === null
  ) {
    return {
      status: "restored",
      composer: {
        ...composer,
        input: item.text,
        draftImages: [...item.draftImages],
        imageUrls: new Map(item.imageUrls),
        documentText: item.documentText,
        documentName: item.documentName,
        draftProjectId: item.draftProjectId,
        isProcessingDocument: false,
        imagePasteGeneration: composer.imagePasteGeneration + 1,
        documentUploadGeneration: composer.documentUploadGeneration + 1
      }
    };
  }

  return {
    status: "requeued",
    composer: {
      ...composer,
      queue: { ...composer.queue, items: [item, ...composer.queue.items] }
    }
  };
}

function combineDraftText(destination: string, source: string, separator: string): string {
  if (!destination) return source;
  if (!source) return destination;
  return `${destination}${separator}${source}`;
}

type PreparedComposerEdit = Readonly<{
  items: ChatQueuedMessage[];
  draftInput: string;
  visibleEdit: Readonly<{ edit: ChatQueuedMessageEdit; input: string }> | null;
}>;

function preserveComposerQueueEdit(composer: ChatComposerDraft): PreparedComposerEdit {
  const edit = composer.queue.edit;
  if (!edit) {
    return { items: composer.queue.items, draftInput: composer.input, visibleEdit: null };
  }

  if (!composer.queue.items.some((item) => item.queueId === edit.queueId)) {
    return {
      items: composer.queue.items,
      draftInput: combineDraftText(discardSharedQueuedMessageEdit(edit), composer.input, "\n"),
      visibleEdit: null
    };
  }

  return {
    items: composer.queue.items,
    draftInput: discardSharedQueuedMessageEdit(edit),
    visibleEdit: { edit, input: composer.input }
  };
}

function foldComposerQueueEditIntoDraft(composer: ChatComposerDraft): PreparedComposerEdit {
  const prepared = preserveComposerQueueEdit(composer);
  if (!prepared.visibleEdit) return prepared;

  // A concurrent destination edit owns the one visible editor. Never turn the
  // source's partially edited text into a promotable queued item without an
  // explicit submit: retain the original item and move both source text fields
  // into the surviving edit's non-promotable stash.
  return {
    items: composer.queue.items,
    draftInput: combineDraftText(
      discardSharedQueuedMessageEdit(prepared.visibleEdit.edit),
      prepared.visibleEdit.input,
      "\n"
    ),
    visibleEdit: null
  };
}

type MergedQueueItems = Readonly<{
  items: ChatQueuedMessage[];
  destinationEditQueueId: string | null;
  sourceEditQueueId: string | null;
}>;

function mergeQueueItems(
  destinationItems: ChatQueuedMessage[],
  sourceItems: ChatQueuedMessage[],
  destinationEditQueueId: string | null,
  sourceEditQueueId: string | null
): MergedQueueItems {
  const usedQueueIds = new Set<string>();
  const usedMessageIds = new Set<string>();
  const makeUniqueId = (candidate: string, used: Set<string>): string => {
    if (!used.has(candidate)) {
      used.add(candidate);
      return candidate;
    }
    let suffix = 2;
    while (used.has(`${candidate}:rekey:${suffix}`)) suffix += 1;
    const unique = `${candidate}:rekey:${suffix}`;
    used.add(unique);
    return unique;
  };
  let normalizedDestinationEditQueueId: string | null = null;
  let normalizedSourceEditQueueId: string | null = null;
  const normalizeIds = (
    item: ChatQueuedMessage,
    origin: "destination" | "source"
  ): ChatQueuedMessage => {
    const originalQueueId = item.queueId;
    const queueId = makeUniqueId(item.queueId, usedQueueIds);
    const messageId = makeUniqueId(item.messageId, usedMessageIds);
    if (
      origin === "destination" &&
      normalizedDestinationEditQueueId === null &&
      originalQueueId === destinationEditQueueId
    ) {
      normalizedDestinationEditQueueId = queueId;
    }
    if (
      origin === "source" &&
      normalizedSourceEditQueueId === null &&
      originalQueueId === sourceEditQueueId
    ) {
      normalizedSourceEditQueueId = queueId;
    }
    return queueId === item.queueId && messageId === item.messageId
      ? item
      : { ...item, queueId, messageId };
  };
  const combined = [
    ...destinationItems.map((item, index) => ({
      item: normalizeIds(item, "destination"),
      origin: 0,
      index
    })),
    ...sourceItems.map((item, index) => ({
      item: normalizeIds(item, "source"),
      origin: 1,
      index
    }))
  ];
  return {
    items: combined
      .sort(
        (left, right) =>
          left.item.createdMs - right.item.createdMs ||
          left.origin - right.origin ||
          left.index - right.index
      )
      .map(({ item }) => item),
    destinationEditQueueId: normalizedDestinationEditQueueId,
    sourceEditQueueId: normalizedSourceEditQueueId
  };
}

function mergeDraftImages(
  destinationImages: File[],
  destinationUrls: Map<File, string>,
  sourceImages: File[],
  sourceUrls: Map<File, string>
): Readonly<{ images: File[]; urls: Map<File, string>; displacedObjectUrls: string[] }> {
  const images = [...destinationImages];
  const knownImages = new Set(images);
  for (const image of sourceImages) {
    if (!knownImages.has(image)) {
      knownImages.add(image);
      images.push(image);
    }
  }

  const urls = new Map(sourceUrls);
  const displacedObjectUrls: string[] = [];
  for (const [file, destinationUrl] of destinationUrls) {
    const sourceUrl = urls.get(file);
    if (sourceUrl && sourceUrl !== destinationUrl) displacedObjectUrls.push(sourceUrl);
    urls.set(file, destinationUrl);
  }
  return { images, urls, displacedObjectUrls };
}

/**
 * Reconciles a draft runtime with an independently materialized conversation.
 * The destination edit remains visible when both runtimes are editing. A lone
 * source edit remains open. When both edit, the source's original item stays
 * unchanged and both source text fields move into the destination edit's stash,
 * keeping the entire FIFO blocked until the user explicitly resolves that edit.
 */
export function mergeChatComposerDraftsForRekey<TComposer extends ChatComposerDraft>(
  source: TComposer,
  destination: TComposer,
  targetScopeKey: string
): MergeChatComposerDraftsResult<TComposer> {
  const preparedDestination = preserveComposerQueueEdit(destination);
  const preparedSource = preparedDestination.visibleEdit
    ? foldComposerQueueEditIntoDraft(source)
    : preserveComposerQueueEdit(source);
  const imageMerge = mergeDraftImages(
    destination.draftImages,
    destination.imageUrls,
    source.draftImages,
    source.imageUrls
  );
  const queueMerge = mergeQueueItems(
    preparedDestination.items,
    preparedSource.items,
    preparedDestination.visibleEdit?.edit.queueId ?? null,
    preparedSource.visibleEdit?.edit.queueId ?? null
  );
  const visibleEdit = preparedDestination.visibleEdit ?? preparedSource.visibleEdit;
  const visibleEditQueueId = preparedDestination.visibleEdit
    ? queueMerge.destinationEditQueueId
    : queueMerge.sourceEditQueueId;
  const mergedDraftInput = combineDraftText(
    preparedDestination.draftInput,
    preparedSource.draftInput,
    "\n"
  );
  const edit =
    visibleEdit && visibleEditQueueId
      ? {
          ...visibleEdit.edit,
          scopeKey: targetScopeKey,
          queueId: visibleEditQueueId,
          stashedDraft: mergedDraftInput
        }
      : null;
  const input = edit && visibleEdit ? visibleEdit.input : mergedDraftInput;

  const composer = {
    ...source,
    ...destination,
    input,
    draftImages: imageMerge.images,
    imageUrls: imageMerge.urls,
    documentText: combineDraftText(destination.documentText, source.documentText, "\n\n"),
    documentName: combineDraftText(destination.documentName, source.documentName, ", "),
    isProcessingDocument: destination.isProcessingDocument || source.isProcessingDocument,
    imagePasteGeneration: Math.max(destination.imagePasteGeneration, source.imagePasteGeneration),
    documentUploadGeneration: Math.max(
      destination.documentUploadGeneration,
      source.documentUploadGeneration
    ),
    queue: { items: queueMerge.items, edit }
  } as TComposer;
  const retainedObjectUrls = new Set(chatComposerObjectUrls(composer));

  return {
    composer,
    displacedObjectUrls: Array.from(new Set(imageMerge.displacedObjectUrls)).filter(
      (url) => !retainedObjectUrls.has(url)
    )
  };
}

export function chatComposerObjectUrls(composer: ChatComposerDraft): string[] {
  const urls = new Set<string>(composer.imageUrls.values());
  for (const item of composer.queue.items) {
    for (const url of item.imageUrls.values()) urls.add(url);
  }
  return Array.from(urls);
}

export function disposeChatComposerObjectUrls(
  composer: ChatComposerDraft,
  revokeObjectUrl: (url: string) => void = URL.revokeObjectURL
): void {
  for (const url of chatComposerObjectUrls(composer)) revokeObjectUrl(url);
}
