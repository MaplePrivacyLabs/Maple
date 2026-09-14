import { createContext, useContext, useLayoutEffect, useMemo, type ReactNode } from "react";
import {
  ChatRuntimeStore,
  type ChatRuntimeKey,
  type ChatRuntimeSnapshot,
  type ChatRuntimeStoreOptions
} from "@/services/chatRuntimeStore";
import { createDeferredDisposalLifecycle } from "@/services/deferredDisposalLifecycle";
import {
  disposeChatComposerObjectUrls,
  emptyChatComposerQueueState,
  type ChatComposerQueueState
} from "@/services/chatComposerQueue";
import { cancelActiveChatRuntimeRuns } from "@/services/chatRuntimeCancellation";

export type ChatPaginationState = {
  oldestItemId: string | undefined;
  isLoadingOlderMessages: boolean;
  hasMoreOlderMessages: boolean;
};

export type ChatComposerState = {
  input: string;
  draftProjectId: string | null;
  draftImages: File[];
  imageUrls: Map<File, string>;
  documentText: string;
  documentName: string;
  isProcessingDocument: boolean;
  attachmentError: string | null;
  audioError: string | null;
  imagePasteGeneration: number;
  documentUploadGeneration: number;
  queue: ChatComposerQueueState;
  pagination: ChatPaginationState;
};

export function createChatComposerState(draftProjectId: string | null = null): ChatComposerState {
  return {
    input: "",
    draftProjectId,
    draftImages: [],
    imageUrls: new Map(),
    documentText: "",
    documentName: "",
    isProcessingDocument: false,
    attachmentError: null,
    audioError: null,
    imagePasteGeneration: 0,
    documentUploadGeneration: 0,
    queue: emptyChatComposerQueueState(),
    pagination: {
      oldestItemId: undefined,
      isLoadingOlderMessages: false,
      hasMoreOlderMessages: false
    }
  };
}

type UntypedChatRuntimeStore = ChatRuntimeStore<unknown, unknown, ChatComposerState>;

const ChatRuntimeContext = createContext<UntypedChatRuntimeStore | null>(null);

export function composerHasRetainedDraft(
  key: ChatRuntimeKey,
  composer: ChatComposerState
): boolean {
  return (
    composer.input.length > 0 ||
    (key.startsWith("draft:") && composer.draftProjectId !== null) ||
    composer.draftImages.length > 0 ||
    composer.documentText.length > 0 ||
    composer.documentName.length > 0 ||
    composer.isProcessingDocument ||
    composer.queue.items.length > 0 ||
    composer.queue.edit !== null
  );
}

function disposeComposerResources(
  snapshot: ChatRuntimeSnapshot<unknown, unknown, ChatComposerState>
): void {
  disposeChatComposerObjectUrls(snapshot.composer);
}

export function createChatRuntimeStore<TConversation = unknown, TMessage = unknown>(
  maxInactiveCompletedEntries = 20
): ChatRuntimeStore<TConversation, TMessage, ChatComposerState> {
  const options: ChatRuntimeStoreOptions<TConversation, TMessage, ChatComposerState> = {
    createComposer: createChatComposerState,
    maxInactiveCompletedEntries,
    canEvict: (snapshot) =>
      !snapshot.isGenerating &&
      !snapshot.assistantStreaming &&
      snapshot.runToken === null &&
      !composerHasRetainedDraft(snapshot.key, snapshot.composer),
    disposeEntry: disposeComposerResources
  };
  return new ChatRuntimeStore(options);
}

/**
 * Owns in-memory chat streams and drafts for one authenticated account scope.
 * The parent keys this provider by user ID, so an account transition disposes
 * controllers and object URLs without making sensitive state module-global.
 */
export function ChatRuntimeProvider({ children }: { children: ReactNode }) {
  const store = useMemo(() => createChatRuntimeStore(), []);
  const disposalLifecycle = useMemo(
    () => createDeferredDisposalLifecycle(() => store.dispose()),
    [store]
  );

  useLayoutEffect(() => {
    const scheduleDisposal = disposalLifecycle.activate();
    return () => {
      // Account-keyed provider teardown must fence an old account's FIFO
      // synchronously. Resource disposal stays deferred for Strict Mode replay,
      // but no queued request may advance during that deferral window.
      cancelActiveChatRuntimeRuns(store);
      scheduleDisposal();
    };
  }, [disposalLifecycle, store]);

  return <ChatRuntimeContext.Provider value={store}>{children}</ChatRuntimeContext.Provider>;
}

export function useChatRuntimeStore<TConversation, TMessage>(): ChatRuntimeStore<
  TConversation,
  TMessage,
  ChatComposerState
> {
  const store = useContext(ChatRuntimeContext);
  if (!store) {
    throw new Error("useChatRuntimeStore must be used within a ChatRuntimeProvider");
  }
  return store as unknown as ChatRuntimeStore<TConversation, TMessage, ChatComposerState>;
}
