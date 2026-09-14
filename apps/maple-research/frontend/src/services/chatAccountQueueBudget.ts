import {
  chatQueuedTextByteLength,
  MAX_CHAT_ACCOUNT_RETAINED_IMAGES,
  MAX_CHAT_QUEUED_IMAGES_PER_ITEM,
  type ChatAccountQueueUsage,
  type ChatComposerDraft,
  type ChatQueuedMessage
} from "./chatComposerQueue";
import { getRegisteredChatCurrentTurnPayloads } from "./chatCurrentTurnRegistry";

type RetainedChatComposer = Readonly<{
  draftImages: readonly File[];
  imageUrls: ReadonlyMap<File, string>;
  documentText: string;
  queue: Readonly<{ items: readonly ChatQueuedMessage[] }>;
}>;

type ChatAccountRuntimeLookup = object & {
  getSnapshots: () => readonly Readonly<{ composer: RetainedChatComposer }>[];
};

export type ChatImageFileSelection = Readonly<{
  files: readonly File[];
  accountLimitExceeded: boolean;
  messageLimitExceeded: boolean;
}>;

function isChatQueuedMessage(payload: unknown): payload is ChatQueuedMessage {
  return Boolean(
    payload &&
    typeof payload === "object" &&
    "queueId" in payload &&
    typeof payload.queueId === "string" &&
    "draftImages" in payload &&
    Array.isArray(payload.draftImages) &&
    "imageUrls" in payload &&
    payload.imageUrls instanceof Map &&
    "documentText" in payload &&
    typeof payload.documentText === "string"
  );
}

/**
 * Counts account-scoped attachment ownership and staged messages exactly once,
 * including a FIFO item temporarily popped into the active send loop.
 */
export function chatAccountQueueUsage(store: ChatAccountRuntimeLookup): ChatAccountQueueUsage {
  const seenFiles = new Set<File>();
  const seenQueueItems = new Set<ChatQueuedMessage>();
  let queuedMessageCount = 0;
  let documentBytes = 0;

  const addFile = (file: File) => {
    if (seenFiles.has(file)) return;
    seenFiles.add(file);
  };
  const addItem = (item: ChatQueuedMessage, countsTowardQueueLimit: boolean) => {
    if (seenQueueItems.has(item)) return;
    seenQueueItems.add(item);
    if (countsTowardQueueLimit) queuedMessageCount += 1;
    documentBytes += chatQueuedTextByteLength(item.documentText);
    for (const file of item.draftImages) addFile(file);
    for (const file of item.imageUrls.keys()) addFile(file);
  };

  for (const { composer } of store.getSnapshots()) {
    documentBytes += chatQueuedTextByteLength(composer.documentText);
    for (const file of composer.draftImages) addFile(file);
    for (const file of composer.imageUrls.keys()) addFile(file);
    for (const item of composer.queue.items) addItem(item, true);
  }
  for (const current of getRegisteredChatCurrentTurnPayloads(store)) {
    if (isChatQueuedMessage(current.payload)) {
      addItem(current.payload, current.countsTowardQueueLimit);
    }
  }

  let attachmentBytes = documentBytes;
  for (const file of seenFiles) attachmentBytes += file.size;
  return { queuedMessageCount, attachmentBytes, imageCount: seenFiles.size };
}

/**
 * Selects the prefix of new image Files that fits both the live-message and
 * account-wide retained-image limits. Existing and repeated File identities
 * are ignored, so callers never need to create a replacement blob URL for an
 * image the composer already owns.
 */
export function selectChatImageFilesForRetention({
  composer,
  candidates,
  accountUsage
}: {
  composer: Pick<ChatComposerDraft, "draftImages" | "imageUrls">;
  candidates: readonly File[];
  accountUsage: ChatAccountQueueUsage;
}): ChatImageFileSelection {
  const existingFiles = new Set(composer.draftImages);
  for (const file of composer.imageUrls.keys()) existingFiles.add(file);

  const uniqueCandidates: File[] = [];
  const seenCandidates = new Set(existingFiles);
  for (const file of candidates) {
    if (seenCandidates.has(file)) continue;
    seenCandidates.add(file);
    uniqueCandidates.push(file);
  }

  const messageCapacity = Math.max(0, MAX_CHAT_QUEUED_IMAGES_PER_ITEM - existingFiles.size);
  const accountCapacity = Math.max(0, MAX_CHAT_ACCOUNT_RETAINED_IMAGES - accountUsage.imageCount);
  const files = uniqueCandidates.slice(0, Math.min(messageCapacity, accountCapacity));

  return {
    files,
    messageLimitExceeded: uniqueCandidates.length > messageCapacity,
    accountLimitExceeded: uniqueCandidates.length > accountCapacity
  };
}
