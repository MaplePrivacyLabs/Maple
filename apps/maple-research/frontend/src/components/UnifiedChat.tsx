import {
  useState,
  useRef,
  useEffect,
  useLayoutEffect,
  useCallback,
  memo,
  useMemo,
  useId,
  useSyncExternalStore
} from "react";
import { flushSync } from "react-dom";
import {
  ArrowUp,
  Plus,
  Image,
  FileText,
  X,
  Mic,
  SquarePen,
  Loader2,
  Globe,
  Expand,
  Shrink,
  Volume2,
  Square,
  LockKeyhole
} from "lucide-react";
import RecordRTC from "recordrtc";
import { useQueryClient } from "@tanstack/react-query";
import { v4 as uuidv4 } from "uuid";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Sidebar, SidebarToggle } from "@/components/Sidebar";
import { ResizableSidebarLayout } from "@/components/ResizableSidebarLayout";
import { MapleWordmark } from "@/components/MapleWordmark";
import { useIsMobile, useIsLandscapeMobile } from "@/utils/utils";
import { fileToDataURL } from "@/utils/file";
import {
  getImageFilesFromClipboardItems,
  maybeReadLinuxTauriClipboardImages
} from "@/utils/imagePaste";
import { truncateMarkdownPreservingLinks } from "@/utils/markdown";
import { useLazyRef } from "@/utils/useLazyRef";
import { useVisibleExternalStore } from "@/utils/useVisibleExternalStore";
import {
  getDocumentProcessingErrorMessage,
  getSupportedDocumentType,
  isNativeDocumentType,
  prepareExtractedDocumentText,
  prepareExtractedPdfText
} from "@/utils/documentUpload";
import { useOpenAI } from "@/ai/useOpenAi";
import { DEFAULT_MODEL_ID, getInitialWebSearchEnabled } from "@/state/LocalStateContext";
import { Markdown, ThinkingBlock } from "@/components/markdown";
import {
  CHAT_COMPOSER_TEXTAREA_CLASS,
  ChatAssistantPendingTurn,
  ChatAssistantTurn,
  ChatComposerSurface,
  ChatDesktopConversationHeader,
  ChatUserTurn
} from "@/components/chat/ChatTurn";
import {
  DiscardQueuedMessageEditButton,
  QUEUED_MESSAGE_EDIT_PLACEHOLDER,
  QueuedComposerMessages
} from "@/components/chat/QueuedComposerMessages";
import { ChatCopyButton } from "@/components/chat/ChatCopyButton";
import { ToolActivityCard } from "@/components/ToolActivityCard";
import {
  continueChatComposerList,
  continueChatComposerListBeforeInput
} from "@/components/chatComposerListContinuation";
import { ModelSelector } from "@/components/ModelSelector";
import { useBillingState, useModelState, useSelectedProjectState } from "@/state/useLocalState";
import { isKnownFreePlan } from "@/billing/billingAccess";
import {
  findOpenSecretInferenceCapacityError,
  OPEN_SECRET_INFERENCE_SEND_LIMIT_HEADER,
  useOpenSecret
} from "@mapleai/sdk";
import { UpgradePromptDialog } from "@/components/UpgradePromptDialog";
import { DocumentPlatformDialog } from "@/components/DocumentPlatformDialog";
import { ContextLimitDialog } from "@/components/ContextLimitDialog";
import { RecordingOverlay } from "@/components/RecordingOverlay";
import { useTTS } from "@/services/tts/TTSContext";
import { extractDocumentContent } from "@/services/documentExtractionService";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { AlertCircle } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger
} from "@/components/ui/dropdown-menu";
import { isLinux, isMacOS, isTauri } from "@/utils/platform";
import { ConversationProjectPicker } from "@/components/ConversationProjectPicker";
import {
  CHAT_HISTORY_TOP_MARGIN_PX,
  ChatHistoryPaginationGate,
  chatHistoryCursorProgressed,
  preferredChatHistoryScrollSnapshot,
  requiredChatHistoryBottomCompensation,
  restoredChatHistoryAnchorScrollTop,
  restoredChatHistoryScrollTop,
  type ChatHistoryScrollSnapshot,
  usesFirstCancelableWheelGestureStart
} from "@/components/chatHistoryPagination";
import {
  ChatProjectionScrollCoordinator,
  chatProjectionScrollTarget,
  projectedUserTurnScrollTop,
  type ChatProjectionScrollLease
} from "@/components/chatProjectionScroll";
import type {
  InputTextContent,
  OutputTextContent,
  TextContent,
  SummaryTextContent,
  RefusalContent,
  InputImageContent,
  ComputerScreenshotContent,
  InputFileContent
} from "openai/resources/conversations/conversations.js";
import type {
  ResponseFunctionWebSearch,
  ResponseFunctionToolCall,
  ResponseFunctionToolCallOutputItem,
  ResponseCreateParamsStreaming,
  ResponseOutputItemAddedEvent,
  ResponseOutputItemDoneEvent,
  ResponseReasoningItem,
  ResponseReasoningTextDeltaEvent,
  ResponseReasoningTextDoneEvent,
  ResponseTextDeltaEvent,
  ResponseTextDoneEvent
} from "openai/resources/responses/responses.js";
import type { Message as OpenAIMessage } from "openai/resources/conversations/conversations.js";
import { usePersistentSidebarState } from "@/contexts/PersistentHomeNavigationContext";
import {
  createChatComposerState,
  useChatRuntimeStore,
  type ChatComposerState
} from "@/contexts/ChatRuntimeContext";
import {
  draftScopeForRuntimeSelection,
  moveRememberedChatDraftToScope,
  rememberChatDraftInScope,
  resumeOrCreateChatDraftKey
} from "@/services/chatDraftSelection";
import {
  createChatDraftKey,
  createConversationChatKey,
  type ChatRuntimeKey,
  type DraftChatRuntimeKey
} from "@/services/chatRuntimeStore";
import {
  canonicalConversationHistoryHref,
  conversationIdFromChatRuntimeKey,
  createChatHistoryEntryForDraft,
  draftRuntimeKeyFromHistoryState,
  historyStateWithDraftRuntimeKey,
  isDraftChatRuntimeKey,
  runtimeKeyForChatLocation,
  shouldProjectMigratedConversation,
  type NewChatNavigationDetail
} from "@/services/chatRuntimeNavigation";
import {
  canAdoptRecordingDestination,
  cleanupRecordingForNavigation,
  cleanupRecordingForTeardown,
  isRecordingOwnershipCurrent
} from "@/services/chatRecordingNavigation";
import { canAdoptAttachmentDestination } from "@/services/chatAttachmentOwnership";
import {
  classifyChatStreamEof,
  createChatStreamDeltaCoalescer,
  flushRegisteredChatStreamDeltas,
  isTerminalChatStreamErrorEvent,
  registerChatStreamDeltaCoalescer,
  unregisterChatStreamDeltaCoalescer,
  type ChatStreamTerminalState
} from "@/services/chatStreamDeltaCoalescer";
import {
  isChatRequestDefinitelyNotDispatchedError,
  isChatResponseCancellationAlreadyTerminalError,
  isChatResponseDefinitelyRejectedError,
  isImageDescriptionUnavailableError
} from "@/services/chatResponseErrors";
import { isChatAccountCredentialMismatchError } from "@/services/chatAccountCredential";
import { chatCursorAfterSendFailure } from "@/services/chatSendFailureRecovery";
import { normalizeChatPollingPage } from "@/services/chatPollingPage";
import {
  classifyChatResponseReconciliation,
  responseIdForChatMessage
} from "@/services/chatResponseReconciliation";
import {
  clearUnresolvedChatResponseMessage,
  getUnresolvedChatResponseMessage,
  registerUnresolvedChatResponseMessage
} from "@/services/chatUnresolvedResponseOwnership";
import {
  chatAccountQueueUsage,
  selectChatImageFilesForRetention
} from "@/services/chatAccountQueueBudget";
import { isChatRuntimeDeletionPending } from "@/services/chatRuntimeDeletionFence";
import {
  clearChatRunQueueHalt,
  isChatRunQueueHaltRequested,
  requestChatRunQueueHalt
} from "@/services/chatRunQueueHalt";
import { chatStoppingRuntimeRegistryFor } from "@/services/chatStoppingRuntimeRegistry";
import {
  registerChatCurrentTurn,
  restoreRegisteredChatTurnBeforeRequest
} from "@/services/chatCurrentTurnRegistry";
import {
  beginChatQueuedMessageEdit,
  cancelChatQueuedMessage,
  chatComposerObjectUrls,
  chatQueuedTextByteLength,
  discardChatQueuedMessageEdit,
  MAX_CHAT_ACCOUNT_RETAINED_ATTACHMENT_BYTES,
  MAX_CHAT_ACCOUNT_RETAINED_IMAGES,
  mergeChatComposerDraftsForRekey,
  recoverDetachedChatComposerDraft,
  takeNextChatQueuedMessage,
  type ChatQueuedMessage,
  type ChatQueuedMessageMetadata
} from "@/services/chatComposerQueue";
import {
  canSubmitChatComposer,
  chatComposerWithInputOverride,
  chatComposerShowsStop,
  planChatComposerSubmission
} from "@/services/chatComposerSend";
import {
  chatToolCallStatus,
  chatToolOutputStatus,
  chatToolTitle,
  chatWebSearchStatus,
  formatChatToolArguments
} from "@/services/chatToolPresentation";
import {
  getRegisteredChatOptimisticMessage,
  markOptimisticMessageIncomplete,
  registerChatOptimisticMessage,
  unregisterChatOptimisticMessage
} from "@/services/chatOptimisticMessageOwnership";
import { toolKindFromName } from "@/services/toolPresentation";
import { withInferenceCapacityRetry } from "@/services/inferenceCapacityRetry";

const CHAT_ALERT_CLASS = "absolute top-16 left-1/2 z-50 w-full max-w-2xl -translate-x-1/2 px-4";
const STREAM_EVENT_DEBUG_STORAGE_KEY = "maple:sse-debug";
const CHAT_ACCOUNT_ATTACHMENT_LIMIT_MESSAGE =
  "Chat drafts and queued attachments can use up to 256 MiB across your account";
const CHAT_ACCOUNT_IMAGE_LIMIT_MESSAGE = `Chat drafts and queued messages can retain up to ${MAX_CHAT_ACCOUNT_RETAINED_IMAGES} images across your account`;
const CHAT_MESSAGE_IMAGE_LIMIT_MESSAGE = "You can attach up to 10 images to a message";
const CHAT_STOP_WAITING_MESSAGE = "Stopping as soon as the response is ready…";
const CHAT_STOP_REQUEST_TIMEOUT_MS = 5000;

function isStreamEventDebugLoggingEnabled(): boolean {
  if (!import.meta.env.DEV || typeof window === "undefined") return false;
  try {
    return window.localStorage.getItem(STREAM_EVENT_DEBUG_STORAGE_KEY) === "1";
  } catch {
    return false;
  }
}

type StateUpdate<T> = T | ((previous: T) => T);

function resolveStateUpdate<T>(previous: T, update: StateUpdate<T>): T {
  return typeof update === "function" ? (update as (value: T) => T)(previous) : update;
}

function canonicalizeConversationHistoryEntry(
  conversationId: string,
  state: unknown = window.history.state
): void {
  const href = canonicalConversationHistoryHref(window.location, conversationId);
  const currentHref = `${window.location.pathname}${window.location.search}${window.location.hash}`;
  if (href === currentHref) return;
  window.history.replaceState(state, "", href);
}

type ConversationContent =
  | InputTextContent
  | OutputTextContent
  | TextContent
  | SummaryTextContent
  | RefusalContent
  | InputImageContent
  | ComputerScreenshotContent
  | InputFileContent
  | ResponseFunctionWebSearch
  | ToolCallItem
  | ToolOutputItem;

type MessageStatus = "completed" | "in_progress" | "incomplete" | "streaming" | "error";
type ExtendedMessage = Omit<OpenAIMessage, "status"> & {
  status?: MessageStatus;
};

type ReasoningContentItem = { type: "reasoning_text"; text: string };
type ReasoningItem = Omit<ResponseReasoningItem, "content" | "status"> & {
  content?: ReasoningContentItem[];
  status?: MessageStatus;
};

type ToolCallItem = {
  id: string;
  type: "tool_call";
  call_id: string;
  name: string;
  arguments: string;
  status?: MessageStatus;
};

type ToolOutputItem = {
  id: string;
  type: "tool_output";
  call_id: string;
  output: string;
  status?: MessageStatus;
};

// Union type for all possible conversation items (messages, tool calls, tool outputs, web search, reasoning)
// This combines OpenAI's native types with response streaming types
type Message =
  | ExtendedMessage
  | (ResponseFunctionWebSearch & { id: string })
  | ToolCallItem
  | ToolOutputItem
  | ReasoningItem;

function queuedChatMessageText(item: ChatQueuedMessage): string {
  return item.documentText + (item.documentText && item.text ? `\n\n${item.text}` : item.text);
}

function queuedChatMessageContent(
  item: ChatQueuedMessage,
  imageUrlForFile: (file: File) => string | undefined
): (InputTextContent | InputImageContent)[] {
  const content: (InputTextContent | InputImageContent)[] = [];
  const text = queuedChatMessageText(item);
  if (text) content.push({ type: "input_text", text });
  for (const file of item.draftImages) {
    const imageUrl = imageUrlForFile(file);
    if (!imageUrl) continue;
    content.push({
      type: "input_image",
      image_url: imageUrl,
      detail: "auto",
      file_id: null
    });
  }
  return content;
}

function promotedChatUserMessage(
  item: ChatQueuedMessage,
  content = queuedChatMessageContent(item, (file) => item.imageUrls.get(file))
): Message {
  return {
    id: item.messageId,
    type: "message",
    role: "user",
    content,
    status: "completed"
  } as unknown as Message;
}

function queuedChatMessageFallbackLabel(item: ChatQueuedMessage): string {
  if (item.documentName) return item.documentName;
  const imageCount = item.draftImages.length;
  return imageCount === 1 ? "1 image" : imageCount > 1 ? `${imageCount} images` : "Queued message";
}

function chatTranscriptObjectUrls(messages: readonly Message[]): string[] {
  const urls = new Set<string>();
  for (const message of messages) {
    if (message.type !== "message" || !Array.isArray(message.content)) continue;
    for (const content of message.content) {
      if (
        content.type === "input_image" &&
        typeof content.image_url === "string" &&
        content.image_url.startsWith("blob:")
      ) {
        urls.add(content.image_url);
      }
    }
  }
  return Array.from(urls);
}

function revokeQueuedChatMessageObjectUrls(
  item: ChatQueuedMessage,
  retainedObjectUrls: ReadonlySet<string> = new Set()
): void {
  for (const url of new Set(item.imageUrls.values())) {
    if (!retainedObjectUrls.has(url)) URL.revokeObjectURL(url);
  }
}

// Helper function to merge messages while ensuring uniqueness by ID
// This prevents duplicate key warnings in React by deduplicating messages
function mergeMessagesById(existingMessages: Message[], newMessages: Message[]): Message[] {
  const messagesMap = new Map<string, Message>();

  // First, add all existing messages
  existingMessages.forEach((msg) => messagesMap.set(msg.id, msg));

  // Then, add/update with new messages (overwrites if ID already exists)
  newMessages.forEach((msg) => messagesMap.set(msg.id, msg));

  // Return as array, maintaining insertion order (Map preserves insertion order)
  return Array.from(messagesMap.values());
}

function updateMessageById(
  messages: Message[],
  messageId: string,
  updater: (message: Message) => Message
): Message[] {
  const messageToUpdate = messages.find((message) => message.id === messageId);
  if (!messageToUpdate) return messages;
  return mergeMessagesById(messages, [updater(messageToUpdate)]);
}

function upsertAssistantTextContent(
  message: ExtendedMessage,
  contentIndex: number,
  text: string,
  status?: ExtendedMessage["status"]
): ExtendedMessage {
  const content = [...(message.content ?? [])];
  const existingPart = content[contentIndex];

  if (
    existingPart &&
    (existingPart.type === "input_text" ||
      existingPart.type === "output_text" ||
      existingPart.type === "text") &&
    "text" in existingPart
  ) {
    content[contentIndex] = {
      ...existingPart,
      text
    };
  } else {
    content[contentIndex] = {
      type: "output_text",
      text,
      annotations: []
    };
  }

  return {
    ...message,
    content,
    ...(status ? { status } : {})
  };
}

function normalizeReasoningItem(item: ResponseReasoningItem | ReasoningItem): ReasoningItem {
  const summary = Array.isArray(item.summary) ? item.summary : [];
  const contentItems = Array.isArray(item.content) ? item.content : [];
  const content = (contentItems.length > 0 ? contentItems : summary)
    .map((contentItem) =>
      typeof contentItem?.text === "string"
        ? ({
            type: "reasoning_text",
            text: contentItem.text
          } as const)
        : null
    )
    .filter((contentItem): contentItem is ReasoningContentItem => contentItem !== null);

  return {
    ...item,
    summary,
    content
  };
}

function normalizeToolCallItem(item: unknown): ToolCallItem | null {
  if (!item || typeof item !== "object" || !("type" in item)) return null;

  if ((item as { type?: string }).type === "tool_call") {
    const toolCall = item as Partial<ToolCallItem>;
    if (typeof toolCall.id !== "string" || typeof toolCall.call_id !== "string") return null;

    return {
      id: toolCall.id,
      type: "tool_call",
      call_id: toolCall.call_id,
      name: typeof toolCall.name === "string" ? toolCall.name : "function",
      arguments:
        typeof toolCall.arguments === "string"
          ? toolCall.arguments
          : JSON.stringify(toolCall.arguments || {}),
      status: toolCall.status
    };
  }

  if ((item as { type?: string }).type === "function_call") {
    const toolCall = item as ResponseFunctionToolCall & { id: string };
    if (!toolCall.id) return null;

    return {
      id: toolCall.id,
      type: "tool_call",
      call_id: toolCall.call_id,
      name: toolCall.name,
      arguments: toolCall.arguments,
      status: toolCall.status
    };
  }

  return null;
}

function normalizeToolOutputItem(item: unknown): ToolOutputItem | null {
  if (!item || typeof item !== "object" || !("type" in item)) return null;

  if ((item as { type?: string }).type === "tool_output") {
    const toolOutput = item as Partial<ToolOutputItem>;
    if (typeof toolOutput.id !== "string" || typeof toolOutput.call_id !== "string") return null;

    return {
      id: toolOutput.id,
      type: "tool_output",
      call_id: toolOutput.call_id,
      output:
        typeof toolOutput.output === "string"
          ? toolOutput.output
          : JSON.stringify(toolOutput.output || ""),
      status: toolOutput.status
    };
  }

  if ((item as { type?: string }).type === "function_call_output") {
    const toolOutput = item as ResponseFunctionToolCallOutputItem & { id: string };
    if (!toolOutput.id) return null;

    return {
      id: toolOutput.id,
      type: "tool_output",
      call_id: toolOutput.call_id,
      output: toolOutput.output,
      status: toolOutput.status
    };
  }

  return null;
}

function isToolCallItem(item: Message): item is ToolCallItem {
  return item.type === "tool_call";
}

function isToolOutputItem(item: Message): item is ToolOutputItem {
  return item.type === "tool_output";
}

function getReasoningContentLength(content?: ReasoningContentItem[]): number {
  return (content ?? []).reduce((total, contentItem) => total + contentItem.text.length, 0);
}

function getMessageContentLength(content?: ExtendedMessage["content"]): number {
  return (content ?? []).reduce((total, contentItem) => {
    if (
      (contentItem.type === "input_text" ||
        contentItem.type === "output_text" ||
        contentItem.type === "text") &&
      "text" in contentItem
    ) {
      return total + contentItem.text.length;
    }

    if (contentItem.type === "input_image") {
      return total + 1;
    }

    return total;
  }, 0);
}

function upsertReasoningTextContent(
  reasoning: ReasoningItem,
  contentIndex: number,
  text: string,
  status?: ReasoningItem["status"]
): ReasoningItem {
  const content = [...(reasoning.content ?? [])];
  content[contentIndex] = {
    type: "reasoning_text",
    text
  };

  return {
    ...reasoning,
    content,
    ...(status ? { status } : {})
  };
}

function normalizeConversationItem(item: unknown): Message | null {
  const toolCall = normalizeToolCallItem(item);
  if (toolCall) return toolCall;

  const toolOutput = normalizeToolOutputItem(item);
  if (toolOutput) return toolOutput;

  if (!item || typeof item !== "object" || !("id" in item) || !("type" in item)) {
    return null;
  }

  const typedItem = item as { type: string };

  if (typedItem.type === "reasoning") {
    return normalizeReasoningItem(item as ResponseReasoningItem | ReasoningItem);
  }

  if (typedItem.type === "message" || typedItem.type === "web_search_call") {
    return item as Message;
  }

  return null;
}

function isAssistantConversationItem(item: Message): boolean {
  return item.type !== "message" || (item as ExtendedMessage).role !== "user";
}

function summarizeConversationItemForLog(item: unknown): Record<string, unknown> {
  const normalizedItem = normalizeConversationItem(item);

  if (!normalizedItem) {
    return {};
  }

  if (normalizedItem.type === "reasoning") {
    return {
      itemType: normalizedItem.type,
      itemId: normalizedItem.id,
      status: normalizedItem.status,
      contentLength: getReasoningContentLength(normalizedItem.content)
    };
  }

  if (normalizedItem.type === "message") {
    const message = normalizedItem as ExtendedMessage;

    return {
      itemType: message.type,
      itemId: message.id,
      role: message.role,
      status: message.status,
      contentParts: message.content?.length ?? 0,
      contentLength: getMessageContentLength(message.content)
    };
  }

  if (normalizedItem.type === "web_search_call") {
    return {
      itemType: normalizedItem.type,
      itemId: normalizedItem.id,
      status: normalizedItem.status
    };
  }

  if (normalizedItem.type === "tool_call") {
    return {
      itemType: normalizedItem.type,
      itemId: normalizedItem.id,
      callId: normalizedItem.call_id,
      name: normalizedItem.name,
      status: normalizedItem.status,
      argumentsLength: normalizedItem.arguments.length
    };
  }

  return {
    itemType: normalizedItem.type,
    itemId: normalizedItem.id,
    callId: normalizedItem.call_id,
    status: normalizedItem.status,
    outputLength: normalizedItem.output.length
  };
}

function summarizeStreamEventForLog(eventType: string, event: unknown): Record<string, unknown> {
  const summary: Record<string, unknown> = {};
  const eventRecord = event as
    | {
        sequence_number?: number;
        item_id?: string;
        response?: {
          id?: string;
          status?: string;
          output?: unknown[];
        };
      }
    | undefined;

  if (typeof eventRecord?.sequence_number === "number") {
    summary.sequenceNumber = eventRecord.sequence_number;
  }

  if (typeof eventRecord?.item_id === "string") {
    summary.itemId = eventRecord.item_id;
  }

  switch (eventType) {
    case "response.created":
    case "response.completed": {
      const response = eventRecord?.response;

      if (response?.id) {
        summary.responseId = response.id;
      }

      if (typeof response?.status === "string") {
        summary.status = response.status;
      }

      if (Array.isArray(response?.output)) {
        summary.outputCount = response.output.length;
      }

      return summary;
    }
    case "response.output_item.added":
    case "response.output_item.done": {
      const itemEvent = event as { output_index?: number; item?: unknown };

      if (typeof itemEvent.output_index === "number") {
        summary.outputIndex = itemEvent.output_index;
      }

      return {
        ...summary,
        ...summarizeConversationItemForLog(itemEvent.item)
      };
    }
    case "response.reasoning_text.delta": {
      const reasoningEvent = event as ResponseReasoningTextDeltaEvent;

      return {
        ...summary,
        contentIndex: reasoningEvent.content_index,
        deltaLength: reasoningEvent.delta.length
      };
    }
    case "response.reasoning_text.done": {
      const reasoningEvent = event as ResponseReasoningTextDoneEvent;

      return {
        ...summary,
        contentIndex: reasoningEvent.content_index,
        textLength: reasoningEvent.text.length
      };
    }
    case "response.output_text.delta": {
      const textEvent = event as ResponseTextDeltaEvent;

      return {
        ...summary,
        contentIndex: textEvent.content_index,
        deltaLength: textEvent.delta.length
      };
    }
    case "response.output_text.done": {
      const textEvent = event as ResponseTextDoneEvent;

      return {
        ...summary,
        contentIndex: textEvent.content_index,
        textLength: textEvent.text.length
      };
    }
    case "tool_call.created": {
      const toolCallEvent = event as {
        tool_call_id?: string;
        name?: string;
        arguments?: string | Record<string, unknown>;
      };

      return {
        ...summary,
        toolCallId: toolCallEvent.tool_call_id,
        name: toolCallEvent.name,
        ...(typeof toolCallEvent.arguments === "string"
          ? { argumentsLength: toolCallEvent.arguments.length }
          : toolCallEvent.arguments && typeof toolCallEvent.arguments === "object"
            ? { argumentKeys: Object.keys(toolCallEvent.arguments) }
            : {})
      };
    }
    case "tool_output.created": {
      const toolOutputEvent = event as {
        tool_output_id?: string;
        tool_call_id?: string;
        output?: string;
      };

      return {
        ...summary,
        toolOutputId: toolOutputEvent.tool_output_id,
        toolCallId: toolOutputEvent.tool_call_id,
        outputLength: toolOutputEvent.output?.length ?? 0
      };
    }
    default:
      return summary;
  }
}

function updateActiveItemStatuses(
  messages: Message[],
  status: "completed" | "error" | "incomplete",
  ownedItemIds?: ReadonlySet<string>
): Message[] {
  const updatedMessages = messages
    .filter((message) => {
      if (ownedItemIds && !ownedItemIds.has(message.id)) return false;
      const currentStatus = (message as { status?: string }).status;
      return (
        currentStatus === "in_progress" ||
        currentStatus === "streaming" ||
        currentStatus === "searching"
      );
    })
    .map((message) => ({ ...message, status }) as Message);

  return updatedMessages.length > 0 ? mergeMessagesById(messages, updatedMessages) : messages;
}

function mergeStreamingConversationItem(messages: Message[], item: Message): Message[] {
  if (item.type === "reasoning") {
    const existingReasoning = messages.find(
      (message): message is ReasoningItem => message.id === item.id && message.type === "reasoning"
    );

    if (!existingReasoning) {
      return mergeMessagesById(messages, [item]);
    }

    const incomingReasoning = item as ReasoningItem;
    const existingContentLength = getReasoningContentLength(existingReasoning.content);
    const incomingContentLength = getReasoningContentLength(incomingReasoning.content);

    return mergeMessagesById(messages, [
      {
        ...existingReasoning,
        ...incomingReasoning,
        content:
          incomingContentLength >= existingContentLength
            ? incomingReasoning.content
            : existingReasoning.content,
        status: incomingReasoning.status ?? existingReasoning.status
      }
    ]);
  }

  if (item.type === "message") {
    const existingMessage = messages.find(
      (message): message is ExtendedMessage => message.id === item.id && message.type === "message"
    );

    if (!existingMessage) {
      return mergeMessagesById(messages, [item]);
    }

    const incomingMessage = item as ExtendedMessage;
    const existingContentLength = getMessageContentLength(existingMessage.content);
    const incomingContentLength = getMessageContentLength(incomingMessage.content);

    return mergeMessagesById(messages, [
      {
        ...existingMessage,
        ...incomingMessage,
        content:
          incomingContentLength >= existingContentLength
            ? incomingMessage.content
            : existingMessage.content,
        status: incomingMessage.status ?? existingMessage.status
      } as Message
    ]);
  }

  if (isToolCallItem(item)) {
    const existingToolCall = messages.find(
      (message): message is ToolCallItem => message.id === item.id && isToolCallItem(message)
    );

    return mergeMessagesById(messages, [
      {
        ...(existingToolCall ?? {}),
        ...item,
        arguments:
          item.arguments.length >= (existingToolCall?.arguments.length ?? 0)
            ? item.arguments
            : (existingToolCall?.arguments ?? item.arguments),
        status: item.status ?? existingToolCall?.status ?? "in_progress"
      }
    ]);
  }

  if (isToolOutputItem(item)) {
    const existingToolOutput = messages.find(
      (message): message is ToolOutputItem => message.id === item.id && isToolOutputItem(message)
    );

    return mergeMessagesById(messages, [
      {
        ...(existingToolOutput ?? {}),
        ...item,
        output:
          item.output.length >= (existingToolOutput?.output.length ?? 0)
            ? item.output
            : (existingToolOutput?.output ?? item.output),
        status: item.status ?? existingToolOutput?.status ?? "in_progress"
      }
    ]);
  }

  return mergeMessagesById(messages, [item]);
}

function reconcileLoadedMessage(serverMessage: Message, localMessage: Message): Message {
  const merged = mergeStreamingConversationItem([serverMessage], localMessage)[0];
  const serverStatus = (serverMessage as { status?: string }).status;
  if (serverStatus === "completed" || serverStatus === "incomplete" || serverStatus === "error") {
    return { ...merged, status: serverStatus } as Message;
  }
  return merged;
}

function mergeLoadedMessagesWithRuntime(loaded: Message[], cached: readonly Message[]): Message[] {
  if (cached.length === 0) return loaded;

  const cachedById = new Map(cached.map((message) => [message.id, message]));
  const loadedIds = new Set(loaded.map((message) => message.id));
  const reconciledLoaded = loaded.map((serverMessage) => {
    const localMessage = cachedById.get(serverMessage.id);
    if (!localMessage) return serverMessage;

    return reconcileLoadedMessage(serverMessage, localMessage);
  });

  // Optimistic and live SSE items may not be checkpointed by the server until
  // terminal events. Keep them in their existing order after the loaded page.
  return [...reconciledLoaded, ...cached.filter((message) => !loadedIds.has(message.id))];
}

function mergePolledMessagesWithRuntime(cached: readonly Message[], polled: Message[]): Message[] {
  if (cached.length === 0) return polled;
  const polledById = new Map(polled.map((message) => [message.id, message]));
  const cachedIds = new Set(cached.map((message) => message.id));
  const reconciledCached = cached.map((localMessage) => {
    const serverMessage = polledById.get(localMessage.id);
    return serverMessage ? reconcileLoadedMessage(serverMessage, localMessage) : localMessage;
  });

  return [...reconciledCached, ...polled.filter((message) => !cachedIds.has(message.id))];
}

// Helper function to convert conversation items - just returns them as-is (flat, no grouping)
// The API already returns items in the correct format (ConversationItem union)
function convertItemsToMessages(items: Array<unknown>): Message[] {
  return items.flatMap((item) => {
    const normalizedItem = normalizeConversationItem(item);

    if (!normalizedItem && item != null) {
      console.warn("Invalid conversation item filtered from API response:", item);
    }

    return normalizedItem ? [normalizedItem] : [];
  });
}

// TTS play button component
function TTSButton({ text, messageId }: { text: string; messageId: string }) {
  const { isPreparing, isPlaying, currentPlayingId, speak, stop } = useTTS();
  const isThisPreparing = isPreparing && currentPlayingId === messageId;
  const isThisPlaying = isPlaying && currentPlayingId === messageId;

  const handleClick = async () => {
    if (isThisPlaying || isThisPreparing) {
      stop();
      return;
    }
    if (!isPreparing) {
      if (isPlaying) {
        stop();
      }
      await speak(text, messageId);
    }
  };

  const isDisabled = isPreparing && !isThisPreparing;

  const ariaLabel = isThisPreparing
    ? "Stop preparing speech"
    : isThisPlaying
      ? "Stop speaking"
      : isPreparing
        ? "Text-to-speech is preparing another message"
        : "Read aloud";

  return (
    <Button
      variant="ghost"
      size="sm"
      className="h-7 w-7 p-0 text-muted-foreground hover:text-foreground"
      onClick={handleClick}
      disabled={isDisabled}
      aria-label={ariaLabel}
      aria-busy={isThisPreparing}
    >
      {isThisPreparing ? (
        <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
      ) : isThisPlaying ? (
        <Square className="h-3.5 w-3.5" aria-hidden="true" />
      ) : (
        <Volume2 className="h-3.5 w-3.5" aria-hidden="true" />
      )}
    </Button>
  );
}

interface Conversation {
  id: string;
  object: "conversation";
  created_at: number;
  project_id?: string | null;
  metadata?: {
    title?: string;
    [key: string]: unknown;
  };
}

function ChatToolDetails({
  input,
  output,
  isExpanded,
  onToggleExpanded
}: {
  input?: string;
  output?: string;
  isExpanded: boolean;
  onToggleExpanded: () => void;
}) {
  const preview = output ? truncateMarkdownPreservingLinks(output, 150) : "";
  const hasMore = Boolean(output && output.length > 150);

  return (
    <>
      {input?.trim() ? (
        <div>
          <p className="mb-1 text-[11px] font-medium uppercase text-muted-foreground">Input</p>
          <pre className="max-h-56 overflow-auto whitespace-pre-wrap break-words rounded-md bg-background/70 px-2 py-1.5 text-xs text-muted-foreground">
            {input}
          </pre>
        </div>
      ) : null}
      {output !== undefined && output.length > 0 ? (
        <div>
          <p className="mb-1 text-[11px] font-medium uppercase text-muted-foreground">Output</p>
          <div className="text-foreground/80">
            <Markdown content={isExpanded ? output : preview} />
            {hasMore ? (
              <button
                type="button"
                onClick={onToggleExpanded}
                aria-expanded={isExpanded}
                className="ml-2 text-xs font-medium text-primary hover:text-primary/80"
              >
                {isExpanded ? "Show less" : "Show more"}
              </button>
            ) : null}
          </div>
        </div>
      ) : null}
    </>
  );
}

// Component to render tool calls
function ToolCallRenderer({
  tool,
  toolOutputs,
  statusOutputs,
  relatedCall
}: {
  tool: ConversationContent;
  toolOutputs?: ToolOutputItem[];
  statusOutputs?: ToolOutputItem[];
  relatedCall?: ToolCallItem;
}) {
  const [isExpanded, setIsExpanded] = useState(false);

  if (tool.type === "web_search_call") {
    const webSearch = tool as ResponseFunctionWebSearch;

    return (
      <ToolActivityCard
        kind="web"
        title="Web Search"
        status={chatWebSearchStatus(webSearch.status)}
      />
    );
  }

  if (tool.type === "tool_call") {
    const functionCall = tool as ToolCallItem;
    const availableToolOutputs = toolOutputs ?? [];
    const relatedToolOutputs = statusOutputs ?? availableToolOutputs;
    const combinedOutput = availableToolOutputs
      .map((toolOutput) => toolOutput.output || "")
      .filter(Boolean)
      .join("\n\n");
    const formattedInput = formatChatToolArguments(functionCall.arguments);
    const hasDetails = Boolean(formattedInput.trim() || combinedOutput.length > 0);
    const cardProps = {
      kind: toolKindFromName(functionCall.name),
      title: chatToolTitle(functionCall.name, functionCall.arguments),
      status: chatToolCallStatus(functionCall.status, relatedToolOutputs)
    };

    if (!hasDetails) return <ToolActivityCard {...cardProps} />;

    return (
      <ToolActivityCard {...cardProps}>
        <ChatToolDetails
          input={formattedInput}
          output={combinedOutput}
          isExpanded={isExpanded}
          onToggleExpanded={() => setIsExpanded(!isExpanded)}
        />
      </ToolActivityCard>
    );
  }

  if (tool.type === "tool_output") {
    const toolOutput = tool as ToolOutputItem;
    const output = toolOutput.output || "";
    const cardProps = relatedCall
      ? {
          kind: toolKindFromName(relatedCall.name),
          title: chatToolTitle(relatedCall.name, relatedCall.arguments)
        }
      : ({ kind: "generic", title: "Tool result" } as const);

    if (!output.length) {
      return <ToolActivityCard {...cardProps} status={chatToolOutputStatus(toolOutput.status)} />;
    }

    return (
      <ToolActivityCard {...cardProps} status={chatToolOutputStatus(toolOutput.status)}>
        <ChatToolDetails
          output={output}
          isExpanded={isExpanded}
          onToggleExpanded={() => setIsExpanded(!isExpanded)}
        />
      </ToolActivityCard>
    );
  }

  return null;
}

// Types for grouping messages into turns
type MessageGroup =
  | { type: "user"; message: ExtendedMessage; id: string }
  | { type: "assistant"; items: Message[]; id: string };

function getUserMessageText(message: ExtendedMessage): string {
  return (
    message.content
      ?.filter((part) => "text" in part && part.text)
      .map((part) => ("text" in part ? part.text : ""))
      .join("\n") || ""
  );
}

function getAssistantGroupText(items: Message[]): string {
  return items
    .filter((item) => item.type === "message")
    .flatMap((item) => {
      const message = item as unknown as ExtendedMessage;
      return (
        message.content
          ?.filter((part) => "text" in part && part.text)
          .map((part) => ("text" in part ? part.text : "")) || []
      );
    })
    .join("");
}

// Memoized message list component to prevent re-renders on input changes
const MessageList = memo(
  ({
    messages,
    isGenerating,
    chatId
  }: {
    messages: Message[];
    isGenerating: boolean;
    chatId?: string;
  }) => {
    const toolCallsByCallId = useMemo(() => {
      const toolCalls = new Map<string, ToolCallItem>();

      messages.forEach((message) => {
        if (isToolCallItem(message)) {
          toolCalls.set(message.call_id, message);
        }
      });

      return toolCalls;
    }, [messages]);

    const toolOutputsByCallId = useMemo(() => {
      const toolOutputs = new Map<string, ToolOutputItem[]>();

      messages.forEach((message) => {
        if (!isToolOutputItem(message)) return;
        const outputs = toolOutputs.get(message.call_id) ?? [];
        outputs.push(message);
        toolOutputs.set(message.call_id, outputs);
      });

      return toolOutputs;
    }, [messages]);

    // Group messages into user turns and assistant turns
    // Assistant turns include: reasoning, tool calls, tool outputs, web search, and assistant messages
    const groupedMessages = useMemo(() => {
      const groups: MessageGroup[] = [];
      let currentAssistantItems: Message[] = [];

      for (const item of messages) {
        // Check if this is a user message
        if (item.type === "message" && (item as unknown as ExtendedMessage).role === "user") {
          // Flush any pending assistant items first
          if (currentAssistantItems.length > 0) {
            groups.push({
              type: "assistant",
              items: [...currentAssistantItems],
              id: `assistant-${currentAssistantItems[0].id}`
            });
            currentAssistantItems = [];
          }
          groups.push({
            type: "user",
            message: item as unknown as ExtendedMessage,
            id: item.id
          });
        } else {
          // This is an assistant-related item (reasoning, tool calls, assistant message, etc.)
          currentAssistantItems.push(item);
        }
      }

      // Don't forget trailing assistant items
      if (currentAssistantItems.length > 0) {
        groups.push({
          type: "assistant",
          items: [...currentAssistantItems],
          id: `assistant-${currentAssistantItems[0].id}`
        });
      }

      return groups;
    }, [messages]);

    const renderAssistantItems = (items: Message[]) => {
      const renderedItems: JSX.Element[] = [];

      for (let index = 0; index < items.length; index++) {
        const item = items[index];
        const itemType = item.type;

        if (itemType === "tool_call") {
          const toolCall = item as ToolCallItem;
          const matchedOutputs: ToolOutputItem[] = [];
          let nextIndex = index + 1;

          while (nextIndex < items.length) {
            const nextItem = items[nextIndex];
            if (isToolOutputItem(nextItem) && nextItem.call_id === toolCall.call_id) {
              matchedOutputs.push(nextItem);
              nextIndex++;
            } else {
              break;
            }
          }

          renderedItems.push(
            <div
              key={item.id}
              data-history-anchor-ids={[item.id, ...matchedOutputs.map((output) => output.id)].join(
                " "
              )}
            >
              <ToolCallRenderer
                tool={toolCall}
                toolOutputs={matchedOutputs}
                statusOutputs={toolOutputsByCallId.get(toolCall.call_id)}
              />
            </div>
          );

          if (matchedOutputs.length > 0) {
            index = nextIndex - 1;
          }

          continue;
        }

        if (itemType === "tool_output") {
          const output = item as ToolOutputItem;
          renderedItems.push(
            <div key={item.id} data-history-anchor-ids={item.id}>
              <ToolCallRenderer tool={output} relatedCall={toolCallsByCallId.get(output.call_id)} />
            </div>
          );
          continue;
        }

        if (itemType === "web_search_call") {
          const webSearch = item as unknown as ResponseFunctionWebSearch;
          renderedItems.push(
            <div key={item.id} data-history-anchor-ids={item.id}>
              <ToolCallRenderer tool={webSearch} />
            </div>
          );
          continue;
        }

        if (itemType === "reasoning") {
          const reasoning = item as ReasoningItem;
          const text = (reasoning.content ?? [])
            .filter((c) => c.type === "reasoning_text")
            .map((c) => c.text)
            .join("");
          const isThinking = reasoning.status === "in_progress" || reasoning.status === "streaming";

          renderedItems.push(
            <div key={item.id} data-history-anchor-ids={item.id} className="mb-2">
              <ThinkingBlock content={text} isThinking={isThinking} />
            </div>
          );
          continue;
        }

        if (itemType === "message") {
          const message = item as unknown as ExtendedMessage;
          if (message.role !== "assistant") continue;

          const isAssistantLoading = message.status === "in_progress";
          if ((!message.content || message.content.length === 0) && !isAssistantLoading) {
            continue;
          }

          renderedItems.push(
            <div key={item.id} data-history-anchor-ids={item.id}>
              <div className="prose prose-sm dark:prose-invert max-w-none">
                <div className="space-y-3">
                  {message.content?.map((part, partIdx) => {
                    if (
                      (part.type === "input_text" ||
                        part.type === "output_text" ||
                        part.type === "text") &&
                      "text" in part &&
                      part.text
                    ) {
                      return (
                        <div key={partIdx}>
                          <Markdown
                            content={part.text}
                            loading={(message as { status?: string }).status === "streaming"}
                            chatId={chatId || ""}
                          />
                        </div>
                      );
                    }
                    if (part.type === "input_image" && "image_url" in part && part.image_url) {
                      return (
                        <div key={partIdx}>
                          <img
                            src={part.image_url}
                            alt={`Attachment ${partIdx + 1}`}
                            className="max-w-full rounded-2xl"
                            style={{ maxHeight: "400px", objectFit: "contain" }}
                          />
                        </div>
                      );
                    }
                    return null;
                  })}
                </div>
              </div>

              {message.status === "in_progress" && (
                <div className="mt-2 flex items-center gap-1 text-muted-foreground">
                  <div className="h-2 w-2 animate-pulse rounded-full bg-foreground/60" />
                  <div className="h-2 w-2 animate-pulse rounded-full bg-foreground/60 delay-75" />
                  <div className="h-2 w-2 animate-pulse rounded-full bg-foreground/60 delay-150" />
                </div>
              )}
              {message.status === "incomplete" && (
                <div className="mt-2 inline-flex items-center gap-2 rounded-2xl bg-muted/50 px-3 py-1.5 text-sm text-muted-foreground">
                  <div className="h-1.5 w-1.5 rounded-full bg-maple-warning" />
                  <span>Chat Canceled</span>
                </div>
              )}
            </div>
          );
        }
      }

      return renderedItems;
    };

    const shouldShowInitialAssistantLoader =
      isGenerating && groupedMessages[groupedMessages.length - 1]?.type !== "assistant";

    return (
      <>
        {groupedMessages.map((group, groupIndex) => {
          if (group.type === "user") {
            const message = group.message;
            if (!message.content || message.content.length === 0) return null;

            const userText = getUserMessageText(message);
            const stackedTop = groupedMessages[groupIndex - 1]?.type === "user";
            const stackedBottom = groupedMessages[groupIndex + 1]?.type === "user";

            return (
              <ChatUserTurn
                key={group.id}
                historyAnchorIds={message.id}
                stackedTop={stackedTop}
                stackedBottom={stackedBottom}
                actions={userText ? <ChatCopyButton text={userText} /> : undefined}
              >
                {message.content.map((part, partIdx) => {
                  if (
                    (part.type === "input_text" ||
                      part.type === "output_text" ||
                      part.type === "text") &&
                    "text" in part &&
                    part.text
                  ) {
                    return (
                      <div key={partIdx}>
                        <Markdown content={part.text} chatId={chatId || ""} />
                      </div>
                    );
                  }
                  if (part.type === "input_image" && "image_url" in part && part.image_url) {
                    return (
                      <div key={partIdx}>
                        <img
                          src={part.image_url}
                          alt={`Attachment ${partIdx + 1}`}
                          className="max-w-full rounded-2xl"
                          style={{ maxHeight: "400px", objectFit: "contain" }}
                        />
                      </div>
                    );
                  }
                  return null;
                })}
              </ChatUserTurn>
            );
          }

          // Assistant group - render all items in one Maple box
          if (group.type === "assistant") {
            const hasContent = group.items.some((item) => {
              if (item.type === "message") {
                const msg = item as unknown as ExtendedMessage;
                return (
                  msg.role === "assistant" &&
                  (msg.content?.length > 0 || msg.status === "in_progress")
                );
              }
              return true; // reasoning, tool calls always count
            });

            if (!hasContent) return null;

            const textContent = getAssistantGroupText(group.items);

            return (
              <ChatAssistantTurn
                key={group.id}
                actions={
                  textContent ? (
                    <>
                      <ChatCopyButton text={textContent} />
                      <TTSButton text={textContent} messageId={group.id} />
                    </>
                  ) : undefined
                }
              >
                {renderAssistantItems(group.items)}
              </ChatAssistantTurn>
            );
          }

          return null;
        })}

        {/* Loading indicator - only show while waiting for the first assistant item (TTFT) */}
        {shouldShowInitialAssistantLoader && <ChatAssistantPendingTurn />}
      </>
    );
  }
);

MessageList.displayName = "MessageList";

export function UnifiedChat({ isVisible = true }: { isVisible?: boolean }) {
  const isMobile = useIsMobile();
  const isLandscapeMobile = useIsLandscapeMobile();
  const isCompactLayout = isMobile || isLandscapeMobile;
  const openai = useOpenAI();
  const { model, hasWhisperModel } = useModelState();
  const { billingStatus } = useBillingState();
  const { selectedProjectId, setSelectedProjectId } = useSelectedProjectState();
  const os = useOpenSecret();
  const isTauriEnv = isTauri();
  const isLinuxEnv = isLinux();
  const isLinuxTauriEnv = isTauriEnv && isLinuxEnv;
  const queryClient = useQueryClient();
  const { playbackError, clearPlaybackError, upgradeRequired, clearUpgradeRequired } = useTTS();
  const runtimeStore = useChatRuntimeStore<Conversation, Message>();
  const runtimeInstanceId = useId();
  const visibleChatOwner = useRef<object>({}).current;
  const responseReconciliationsInFlightRef = useRef(new Set<string>());

  const [initialRuntimeSelection] = useState(() => {
    const params = new URLSearchParams(window.location.search);
    const urlConversationId = params.get("conversation_id") || undefined;
    const initialDraftProjectId = selectedProjectId ?? null;
    const runtimeKey = runtimeKeyForChatLocation(urlConversationId, window.history.state, () =>
      resumeOrCreateChatDraftKey(runtimeStore, initialDraftProjectId, () =>
        createChatDraftKey(`unified-chat-${runtimeInstanceId}`)
      )
    );
    return {
      runtimeKey,
      conversationId:
        urlConversationId ?? conversationIdFromChatRuntimeKey(runtimeStore.resolveKey(runtimeKey))
    };
  });
  const [chatId, setChatId] = useState<string | undefined>(initialRuntimeSelection.conversationId);
  const [activeRuntimeKey, setActiveRuntimeKey] = useState<ChatRuntimeKey>(
    initialRuntimeSelection.runtimeKey
  );
  // Runtime selection is updated synchronously before the matching React state
  // update commits. If the old chat is deleted in that gap, render the store's
  // selected replacement instead of recreating the deleted runtime.
  const renderedRuntimeKey = runtimeStore.get(activeRuntimeKey)
    ? activeRuntimeKey
    : (runtimeStore.getActiveKey() ?? activeRuntimeKey);
  runtimeStore.ensure(renderedRuntimeKey, {
    composer: createChatComposerState(selectedProjectId ?? null)
  });
  const activeRuntimeKeyRef = useRef(renderedRuntimeKey);

  const subscribeToActiveRuntime = useCallback(
    (listener: () => void) => runtimeStore.subscribeKey(renderedRuntimeKey, listener),
    [renderedRuntimeKey, runtimeStore]
  );
  const getActiveRuntimeSnapshot = useCallback(() => {
    const snapshot = runtimeStore.get(renderedRuntimeKey);
    if (!snapshot) throw new Error(`Missing chat runtime for ${renderedRuntimeKey}`);
    return snapshot;
  }, [renderedRuntimeKey, runtimeStore]);
  const activeRuntime = useVisibleExternalStore(
    isVisible,
    subscribeToActiveRuntime,
    getActiveRuntimeSnapshot,
    getActiveRuntimeSnapshot
  );

  useLayoutEffect(() => {
    activeRuntimeKeyRef.current = activeRuntime.key;
  }, [activeRuntime.key]);

  useLayoutEffect(() => {
    if (!isVisible) return;
    runtimeStore.select(renderedRuntimeKey);
    const lease = runtimeStore.claimVisibleChat(visibleChatOwner, renderedRuntimeKey);
    return () => runtimeStore.releaseVisibleChat(lease);
  }, [isVisible, renderedRuntimeKey, runtimeStore, visibleChatOwner]);

  useLayoutEffect(() => {
    if (!isVisible) return;
    const canonicalKey = runtimeStore.resolveKey(renderedRuntimeKey);
    const canonicalConversationId = conversationIdFromChatRuntimeKey(canonicalKey);
    if (canonicalConversationId) {
      canonicalizeConversationHistoryEntry(canonicalConversationId);
      return;
    }
    if (!isDraftChatRuntimeKey(canonicalKey)) return;
    const draftProjectId = activeRuntime.composer.draftProjectId;
    rememberChatDraftInScope(runtimeStore, canonicalKey, draftProjectId);
    if (selectedProjectId !== draftProjectId) setSelectedProjectId(draftProjectId);
    if (draftRuntimeKeyFromHistoryState(window.history.state) === canonicalKey) return;

    window.history.replaceState(
      historyStateWithDraftRuntimeKey(window.history.state, canonicalKey),
      "",
      window.location.href
    );
  }, [
    activeRuntime.composer.draftProjectId,
    isVisible,
    renderedRuntimeKey,
    runtimeStore,
    selectedProjectId,
    setSelectedProjectId
  ]);

  const isRuntimeSelected = useCallback(
    (key: ChatRuntimeKey) => runtimeStore.isChatVisible(key),
    [runtimeStore]
  );

  const updateComposerForKey = useCallback(
    (key: ChatRuntimeKey, updater: (composer: ChatComposerState) => ChatComposerState) => {
      if (!runtimeStore.get(key)) return false;
      runtimeStore.update(key, (snapshot) => ({
        ...snapshot,
        composer: updater(snapshot.composer)
      }));
      return true;
    },
    [runtimeStore]
  );

  const updateAttachmentComposerForKey = useCallback(
    (key: ChatRuntimeKey, updater: (composer: ChatComposerState) => ChatComposerState) => {
      if (!runtimeStore.get(key)) return false;
      let didMutate = false;
      runtimeStore.update(key, (snapshot) => {
        if (snapshot.composer.queue.edit) return snapshot;
        const composer = updater(snapshot.composer);
        didMutate = composer !== snapshot.composer;
        return didMutate ? { ...snapshot, composer } : snapshot;
      });
      return didMutate;
    },
    [runtimeStore]
  );

  // The active view is only a projection. Background conversations keep their
  // own messages, composer, cursors, and run lifecycle in runtimeStore.
  const conversation = activeRuntime.conversation;
  const messages = activeRuntime.messages as Message[];
  const input = activeRuntime.composer.input;
  const draftProjectId = activeRuntime.composer.draftProjectId;
  const isGenerating = activeRuntime.isGenerating;
  const queuedMessages = activeRuntime.composer.queue.items;
  const queueEdit = activeRuntime.composer.queue.edit;
  const stoppingRuntimeRegistry = useMemo(
    () => chatStoppingRuntimeRegistryFor(runtimeStore),
    [runtimeStore]
  );
  useSyncExternalStore(
    stoppingRuntimeRegistry.subscribe,
    stoppingRuntimeRegistry.getSnapshot,
    stoppingRuntimeRegistry.getSnapshot
  );
  const isStopping = Array.from(stoppingRuntimeRegistry.getEntries()).some(
    ([key, runTokens]) =>
      runTokens.size > 0 &&
      runtimeStore.resolveKey(key) === runtimeStore.resolveKey(activeRuntime.key)
  );
  const [isSidebarOpen, setIsSidebarOpen] = usePersistentSidebarState(isCompactLayout);
  const [isSidebarTransitioning, setIsSidebarTransitioning] = useState(false);
  const error = activeRuntime.error;
  const [titleJustUpdatedKey, setTitleJustUpdatedKey] = useState<ChatRuntimeKey | null>(null);
  const titleJustUpdated = Boolean(
    titleJustUpdatedKey &&
    runtimeStore.resolveKey(titleJustUpdatedKey) === runtimeStore.resolveKey(activeRuntimeKey)
  );

  // Pagination states
  const { oldestItemId, isLoadingOlderMessages, hasMoreOlderMessages } =
    activeRuntime.composer.pagination;

  // Attachment states
  const {
    draftImages,
    imageUrls,
    documentText,
    documentName,
    isProcessingDocument,
    attachmentError,
    audioError
  } = activeRuntime.composer;
  const editedQueuedMessage = queueEdit
    ? queuedMessages.find((item) => item.queueId === queueEdit.queueId)
    : undefined;
  const canSubmitMessage = canSubmitChatComposer({
    text: input,
    hasAttachments: queueEdit
      ? Boolean(editedQueuedMessage?.draftImages.length || editedQueuedMessage?.documentText)
      : Boolean(draftImages.length || documentText),
    hasQueuedMessages: queuedMessages.length > 0,
    isEditingQueuedMessage: Boolean(queueEdit),
    hasActiveRun: isGenerating,
    isProcessingDocument,
    isStopping
  });
  const showsStop = chatComposerShowsStop(isGenerating, isStopping);

  const setConversationForKey = useCallback(
    (key: ChatRuntimeKey, update: StateUpdate<Conversation | null>) => {
      if (!runtimeStore.get(key)) return false;
      runtimeStore.update(key, (snapshot) => ({
        ...snapshot,
        conversation: resolveStateUpdate(snapshot.conversation, update)
      }));
      return true;
    },
    [runtimeStore]
  );
  const setErrorForKey = useCallback(
    (key: ChatRuntimeKey, update: StateUpdate<string | null>) => {
      if (!runtimeStore.get(key)) return false;
      runtimeStore.update(key, (snapshot) => ({
        ...snapshot,
        error: resolveStateUpdate(snapshot.error, update)
      }));
      return true;
    },
    [runtimeStore]
  );
  const setLastSeenItemIdForKey = useCallback(
    (key: ChatRuntimeKey, update: StateUpdate<string | undefined>) => {
      if (!runtimeStore.get(key)) return false;
      runtimeStore.update(key, (snapshot) => ({
        ...snapshot,
        lastSeenItemId: resolveStateUpdate(snapshot.lastSeenItemId, update)
      }));
      return true;
    },
    [runtimeStore]
  );
  const setInputForKey = useCallback(
    (key: ChatRuntimeKey, update: StateUpdate<string>) =>
      updateComposerForKey(key, (composer) => ({
        ...composer,
        input: resolveStateUpdate(composer.input, update)
      })),
    [updateComposerForKey]
  );
  const setInput = useCallback(
    (update: StateUpdate<string>) => setInputForKey(activeRuntimeKeyRef.current, update),
    [setInputForKey]
  );
  const setDraftProjectId = useCallback(
    (update: StateUpdate<string | null>) => {
      const key = activeRuntimeKeyRef.current;
      const snapshot = runtimeStore.get(key);
      if (!snapshot) return;
      const nextDraftProjectId = resolveStateUpdate(snapshot.composer.draftProjectId, update);
      if (moveRememberedChatDraftToScope(runtimeStore, key, nextDraftProjectId)) return;

      updateComposerForKey(key, (composer) => ({
        ...composer,
        draftProjectId: nextDraftProjectId
      }));
    },
    [runtimeStore, updateComposerForKey]
  );
  const [upgradeDialogOpen, setUpgradeDialogOpen] = useState(false);
  const [upgradeFeature, setUpgradeFeature] = useState<
    "image" | "document" | "voice" | "tts" | "usage" | "tokens"
  >("image");
  const [documentPlatformDialogOpen, setDocumentPlatformDialogOpen] = useState(false);
  const [contextLimitDialogOpen, setContextLimitDialogOpen] = useState(false);
  const ttsAccessDeniedFeature =
    billingStatus === null || isKnownFreePlan(billingStatus) ? "tts" : "usage";

  useEffect(() => {
    if (!isVisible) {
      if (upgradeRequired) clearUpgradeRequired();
      if (upgradeDialogOpen && upgradeFeature === "tts") setUpgradeDialogOpen(false);
      return;
    }
    if (!upgradeRequired) return;

    setUpgradeFeature(ttsAccessDeniedFeature);
    setUpgradeDialogOpen(true);
    clearUpgradeRequired();
  }, [
    clearUpgradeRequired,
    isVisible,
    ttsAccessDeniedFeature,
    upgradeDialogOpen,
    upgradeFeature,
    upgradeRequired
  ]);

  // Audio recording states
  const [isRecording, setIsRecording] = useState(false);
  const [isTranscribing, setIsTranscribing] = useState(false);
  const [isProcessingSend, setIsProcessingSend] = useState(false);
  const [recordingOwnerKey, setRecordingOwnerKey] = useState<ChatRuntimeKey | null>(null);
  const isRecordingForActive = Boolean(
    isRecording && recordingOwnerKey && isRuntimeSelected(recordingOwnerKey)
  );

  // Web search toggle state - persisted in localStorage, billing-aware initial default
  const [isWebSearchEnabled, setIsWebSearchEnabled] = useState(getInitialWebSearchEnabled);

  // Fullscreen mode for power users - persisted in localStorage
  const [isFullscreen, setIsFullscreen] = useState(() => {
    return localStorage.getItem("chatFullscreen") === "true";
  });
  const [isFullscreenAnimating, setIsFullscreenAnimating] = useState(false);

  const wasLandscapeMobileRef = useRef(isLandscapeMobile);

  // Close an already-open sidebar when the current view rotates into a short landscape layout.
  // A route that mounts while already compact keeps the shared sidebar state during a mode switch.
  useEffect(() => {
    const enteredLandscapeMobile = isLandscapeMobile && !wasLandscapeMobileRef.current;
    wasLandscapeMobileRef.current = isLandscapeMobile;
    if (enteredLandscapeMobile && isSidebarOpen) {
      setIsSidebarOpen(false);
    }
  }, [isLandscapeMobile, isSidebarOpen, setIsSidebarOpen]);

  // Save fullscreen preference to localStorage when it changes
  useEffect(() => {
    localStorage.setItem("chatFullscreen", isFullscreen.toString());
  }, [isFullscreen]);

  // Toggle fullscreen with animation
  const toggleFullscreen = useCallback(() => {
    setIsFullscreenAnimating(true);
    setIsFullscreen((prev) => !prev);
    // Reset animation state after transition completes
    setTimeout(() => setIsFullscreenAnimating(false), 300);
  }, []);

  // Scroll state
  const [isUserScrolling, setIsUserScrolling] = useState(false);
  const isUserScrollingRef = useRef(false);
  const prevStreamingRef = useRef(false);
  const projectionScrollCoordinatorRef = useLazyRef(
    () => new ChatProjectionScrollCoordinator<ChatRuntimeKey>()
  );
  const historyPaginationLifecycle = useMemo(
    () => ({ runtimeKey: renderedRuntimeKey, gate: new ChatHistoryPaginationGate() }),
    [renderedRuntimeKey]
  );
  const historyPaginationGate = historyPaginationLifecycle.gate;
  const pendingHistoryScrollRestoreRef = useRef<ChatHistoryScrollSnapshot | null>(null);
  const pendingHistoryScrollRestoreKeyRef = useRef<ChatRuntimeKey | null>(null);
  const wheelGestureEndTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const macOSWheelGestureStartPendingRef = useRef(false);
  const macOSPreviousWheelCancelableRef = useRef<boolean | null>(null);
  const touchGestureEndTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const keyIntentTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const previousTouchYRef = useRef<number | null>(null);
  const touchHistoryGestureActiveRef = useRef(false);
  const pointerHistoryGestureActiveRef = useRef(false);
  const previousPointerScrollTopRef = useRef(0);
  const suppressedHistoryScrollEndsRef = useRef(0);
  const [newPolledMessagesOwnerKey, setNewPolledMessagesOwnerKey] = useState<ChatRuntimeKey | null>(
    null
  );
  const recorderRef = useRef<RecordRTC | null>(null);
  const streamRef = useRef<MediaStream | null>(null);
  const billingRefreshTimeoutsRef = useLazyRef(() => new Set<ReturnType<typeof setTimeout>>());
  const fileInputRef = useRef<HTMLInputElement>(null);
  const documentInputRef = useRef<HTMLInputElement>(null);
  const fileInputOwnerKeyRef = useRef<ChatRuntimeKey | null>(null);
  const documentInputOwnerKeyRef = useRef<ChatRuntimeKey | null>(null);
  const recordingOwnerKeyRef = useRef<ChatRuntimeKey | null>(null);
  const recordingSessionTokenRef = useRef(0);

  // Refs
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const placeComposerCaretAtEndRef = useRef(true);
  const messagesEndRef = useRef<HTMLDivElement>(null);
  const chatContainerRef = useRef<HTMLDivElement>(null);
  const historyTopSentinelRef = useRef<HTMLDivElement>(null);
  const historyBottomCompensationRef = useRef<HTMLDivElement>(null);
  const activeConversationLoadRef = useLazyRef(() => new Map<ChatRuntimeKey, number>());

  const stopRecordingForNavigation = useCallback(
    (destinationKey: ChatRuntimeKey) => {
      const rawOwnerKey = recordingOwnerKeyRef.current;
      const ownerKey = rawOwnerKey ? runtimeStore.resolveKey(rawOwnerKey) : null;
      const ownerSessionToken = recordingSessionTokenRef.current;
      const recorder = recorderRef.current;
      const stream = streamRef.current;
      const result = cleanupRecordingForNavigation({
        ownerKey,
        destinationKey: runtimeStore.resolveKey(destinationKey),
        recorder: recorder
          ? { stopRecording: (callback) => recorder.stopRecording(callback) }
          : null,
        stream,
        clearOwnership: () => {
          const currentOwnerKey = recordingOwnerKeyRef.current;
          if (
            currentOwnerKey &&
            runtimeStore.resolveKey(currentOwnerKey) === ownerKey &&
            recordingSessionTokenRef.current === ownerSessionToken
          ) {
            recordingOwnerKeyRef.current = null;
            recordingSessionTokenRef.current += 1;
            setRecordingOwnerKey(null);
            setIsRecording(false);
            setIsTranscribing(false);
            setIsProcessingSend(false);
          }
        },
        clearRecorder: () => {
          if (recorderRef.current === recorder) recorderRef.current = null;
        },
        clearStream: () => {
          if (streamRef.current === stream) streamRef.current = null;
        }
      });
      if (result.errors.length > 0) {
        console.error("Failed to fully stop recording during chat navigation:", result.errors);
      }
    },
    [runtimeStore]
  );

  useEffect(
    () => () => {
      const recorder = recorderRef.current;
      const stream = streamRef.current;
      const result = cleanupRecordingForTeardown({
        recorder: recorder
          ? { stopRecording: (callback) => recorder.stopRecording(callback) }
          : null,
        stream,
        clearOwnership: () => {
          recordingOwnerKeyRef.current = null;
          recordingSessionTokenRef.current += 1;
        },
        clearRecorder: () => {
          if (recorderRef.current === recorder) recorderRef.current = null;
        },
        clearStream: () => {
          if (streamRef.current === stream) streamRef.current = null;
        }
      });
      if (result.errors.length > 0) {
        console.error("Failed to fully clean up recording during teardown:", result.errors);
      }
    },
    []
  );

  const clearHistoryBottomCompensation = useCallback(() => {
    if (historyBottomCompensationRef.current) {
      historyBottomCompensationRef.current.style.height = "0px";
    }
  }, []);

  useLayoutEffect(() => {
    clearHistoryBottomCompensation();
  }, [renderedRuntimeKey, clearHistoryBottomCompensation]);

  useLayoutEffect(() => {
    pendingHistoryScrollRestoreRef.current = null;
    pendingHistoryScrollRestoreKeyRef.current = null;
    macOSWheelGestureStartPendingRef.current = false;
    macOSPreviousWheelCancelableRef.current = null;
    previousTouchYRef.current = null;
    touchHistoryGestureActiveRef.current = false;
    pointerHistoryGestureActiveRef.current = false;
    suppressedHistoryScrollEndsRef.current = 0;

    if (wheelGestureEndTimeoutRef.current) {
      clearTimeout(wheelGestureEndTimeoutRef.current);
      wheelGestureEndTimeoutRef.current = null;
    }
    if (keyIntentTimeoutRef.current) {
      clearTimeout(keyIntentTimeoutRef.current);
      keyIntentTimeoutRef.current = null;
    }
    if (touchGestureEndTimeoutRef.current) {
      clearTimeout(touchGestureEndTimeoutRef.current);
      touchGestureEndTimeoutRef.current = null;
    }

    return () => {
      macOSWheelGestureStartPendingRef.current = false;
      macOSPreviousWheelCancelableRef.current = null;
      suppressedHistoryScrollEndsRef.current = 0;
      if (wheelGestureEndTimeoutRef.current) {
        clearTimeout(wheelGestureEndTimeoutRef.current);
        wheelGestureEndTimeoutRef.current = null;
      }
      if (keyIntentTimeoutRef.current) {
        clearTimeout(keyIntentTimeoutRef.current);
        keyIntentTimeoutRef.current = null;
      }
      if (touchGestureEndTimeoutRef.current) {
        clearTimeout(touchGestureEndTimeoutRef.current);
        touchGestureEndTimeoutRef.current = null;
      }
    };
  }, [renderedRuntimeKey]);

  // Auto-resize textarea
  useEffect(() => {
    if (textareaRef.current) {
      textareaRef.current.style.height = "auto";
      const scrollHeight = textareaRef.current.scrollHeight;
      textareaRef.current.style.height = `${Math.min(scrollHeight, 200)}px`;
    }
  }, [input]);

  // Cleanup billing refresh timeout on unmount
  useEffect(() => {
    const billingRefreshTimeouts = billingRefreshTimeoutsRef.current;
    return () => {
      for (const timeout of billingRefreshTimeouts) clearTimeout(timeout);
      billingRefreshTimeouts.clear();
    };
  }, [billingRefreshTimeoutsRef]);

  // Auto-focus textbox on desktop (not mobile/landscape-mobile to avoid keyboard popup interrupting reading)
  // Focus when: app launches, new chat, conversation loads, or assistant finishes streaming
  useLayoutEffect(() => {
    placeComposerCaretAtEndRef.current = true;
  }, [renderedRuntimeKey]);

  useEffect(() => {
    // Skip on compact layouts (mobile + landscape mobile) to avoid keyboard popup
    if (isCompactLayout) return;

    // Focus when not generating and textbox is not disabled
    if (!isGenerating && textareaRef.current && !textareaRef.current.disabled) {
      // Small delay to ensure DOM is ready
      const focusTimeout = setTimeout(() => {
        const textarea = textareaRef.current;
        if (!textarea) return;

        textarea.focus();
        const shouldPlaceCaretAtEnd = placeComposerCaretAtEndRef.current;
        placeComposerCaretAtEndRef.current = false;
        if (shouldPlaceCaretAtEnd && textarea.value.length > 0) {
          const end = textarea.value.length;
          textarea.setSelectionRange(end, end);
        }
      }, 100);

      return () => clearTimeout(focusTimeout);
    }
  }, [isCompactLayout, isGenerating, messages.length, chatId, renderedRuntimeKey]);

  // Improved scroll detection - track if user is near bottom
  const handleScroll = useCallback(() => {
    const container = chatContainerRef.current;
    if (!container) return;

    const compensation = historyBottomCompensationRef.current;
    const compensationHeight = compensation?.offsetHeight ?? 0;
    if (
      compensation &&
      compensationHeight > 0 &&
      container.scrollTop <= container.scrollHeight - compensationHeight - container.clientHeight
    ) {
      clearHistoryBottomCompensation();
    }

    const { scrollTop, scrollHeight, clientHeight } = container;
    const isNearBottom = scrollHeight - scrollTop - clientHeight < 100;
    setIsUserScrolling(!isNearBottom);
  }, [clearHistoryBottomCompensation]);

  useEffect(() => {
    isUserScrollingRef.current = isUserScrolling;
  }, [isUserScrolling]);

  // Scroll to bottom helper
  const scrollToBottom = useCallback(
    (behavior: ScrollBehavior = "smooth") => {
      if (chatContainerRef.current) {
        clearHistoryBottomCompensation();
        chatContainerRef.current.scrollTo({
          top: chatContainerRef.current.scrollHeight,
          behavior
        });
      }
    },
    [clearHistoryBottomCompensation]
  );

  const resolveScrollProjectionKey = useCallback(
    (key: ChatRuntimeKey) => runtimeStore.resolveKey(key),
    [runtimeStore]
  );

  const captureHistoryScrollSnapshot = useCallback(
    (runtimeKey: ChatRuntimeKey): ChatHistoryScrollSnapshot | null => {
      if (!isRuntimeSelected(runtimeKey)) return null;

      const container = chatContainerRef.current;
      if (!container) return null;

      const containerRect = container.getBoundingClientRect();
      const firstVisibleAnchor = Array.from(
        container.querySelectorAll<HTMLElement>("[data-history-anchor-ids]")
      ).find((candidate) => {
        const candidateRect = candidate.getBoundingClientRect();
        return candidateRect.bottom > containerRect.top && candidateRect.top < containerRect.bottom;
      });
      const anchorId = firstVisibleAnchor?.dataset.historyAnchorIds?.split(" ").find(Boolean);
      const anchorOffset = firstVisibleAnchor
        ? firstVisibleAnchor.getBoundingClientRect().top - containerRect.top
        : undefined;

      return {
        scrollTop: container.scrollTop,
        scrollHeight: container.scrollHeight,
        anchorId,
        anchorOffset
      };
    },
    [isRuntimeSelected]
  );

  const runForProjectedRuntime = useCallback(
    (lease: ChatProjectionScrollLease<ChatRuntimeKey> | null, action: () => void) => {
      if (
        !lease ||
        !isRuntimeSelected(lease.ownerKey) ||
        !projectionScrollCoordinatorRef.current.ownsLease(lease, resolveScrollProjectionKey)
      ) {
        return false;
      }

      action();
      return true;
    },
    [isRuntimeSelected, projectionScrollCoordinatorRef, resolveScrollProjectionKey]
  );

  const lastMessageId = messages.length > 0 ? messages[messages.length - 1].id : null;
  const prevLastMessageId = useRef(lastMessageId);
  const hasStreamingMessage = messages.some(
    (message) =>
      message.type === "message" && (message as { status?: string }).status === "streaming"
  );

  const getScrollDebugSnapshot = useCallback(() => {
    const container = chatContainerRef.current;

    if (!container) {
      return null;
    }

    const { scrollTop, scrollHeight, clientHeight } = container;

    return {
      scrollTop: Math.round(scrollTop),
      scrollHeight: Math.round(scrollHeight),
      clientHeight: Math.round(clientHeight),
      distanceFromBottom: Math.max(0, Math.round(scrollHeight - scrollTop - clientHeight))
    };
  }, []);

  const [streamEventDebugLoggingEnabled] = useState(isStreamEventDebugLoggingEnabled);
  const logStreamEvent = useCallback(
    (
      runtimeKey: ChatRuntimeKey,
      runToken: number,
      streamStartedAt: number,
      eventIndex: number,
      eventType: string,
      event: unknown
    ) => {
      if (!streamEventDebugLoggingEnabled) return;

      const includeScrollSnapshot =
        eventType === "response.output_item.added" ||
        eventType === "response.output_item.done" ||
        eventType === "tool_call.created" ||
        eventType === "tool_output.created";

      console.log("🔵 SSE Event", {
        at: new Date().toISOString(),
        elapsedMs: Math.round(performance.now() - streamStartedAt),
        runtimeKey: runtimeStore.resolveKey(runtimeKey),
        runToken,
        eventIndex,
        eventType,
        ...summarizeStreamEventForLog(eventType, event),
        ...(includeScrollSnapshot && isRuntimeSelected(runtimeKey)
          ? {
              isUserScrolling: isUserScrollingRef.current,
              scroll: getScrollDebugSnapshot()
            }
          : {})
      });
    },
    [getScrollDebugSnapshot, isRuntimeSelected, runtimeStore, streamEventDebugLoggingEnabled]
  );

  // Attach scroll listener
  useEffect(() => {
    const container = chatContainerRef.current;
    if (!container) return;

    container.addEventListener("scroll", handleScroll);
    // Initial check
    handleScroll();

    return () => container.removeEventListener("scroll", handleScroll);
  }, [handleScroll]);

  useLayoutEffect(() => {
    if (isLoadingOlderMessages || !pendingHistoryScrollRestoreRef.current) return;

    const container = chatContainerRef.current;
    const snapshot = pendingHistoryScrollRestoreRef.current;
    const ownerKey = pendingHistoryScrollRestoreKeyRef.current;
    pendingHistoryScrollRestoreRef.current = null;
    pendingHistoryScrollRestoreKeyRef.current = null;

    if (!container || !ownerKey || !isRuntimeSelected(ownerKey)) return;

    let restoredScrollTop = restoredChatHistoryScrollTop(snapshot, container.scrollHeight);

    const { anchorId, anchorOffset } = snapshot;
    if (anchorId && anchorOffset !== undefined) {
      const anchor = Array.from(
        container.querySelectorAll<HTMLElement>("[data-history-anchor-ids]")
      ).find((candidate) => candidate.dataset.historyAnchorIds?.split(" ").includes(anchorId));

      if (anchor) {
        const nextAnchorOffset =
          anchor.getBoundingClientRect().top - container.getBoundingClientRect().top;
        restoredScrollTop = restoredChatHistoryAnchorScrollTop(
          container.scrollTop,
          anchorOffset,
          nextAnchorOffset
        );
      }
    }

    const compensation = historyBottomCompensationRef.current;
    const currentCompensation = compensation?.offsetHeight ?? 0;
    const missingScrollRange = requiredChatHistoryBottomCompensation(
      restoredScrollTop,
      container.scrollHeight - currentCompensation,
      container.clientHeight
    );
    if (compensation) {
      // scrollHeight is integer-rounded, so keep one extra pixel available rather than
      // allowing the browser to clamp the restored fractional scroll position.
      compensation.style.height = missingScrollRange > 0 ? `${missingScrollRange + 1}px` : "0px";
    }

    const previousScrollTop = container.scrollTop;
    container.scrollTop = restoredScrollTop;
    if (Math.abs(container.scrollTop - previousScrollTop) > 0.5) {
      // WKWebView emits `scrollend` for this programmatic anchor restore even
      // when trackpad momentum is still active. Ignore that one synthetic end;
      // the real gesture end (or the compatibility timer) must release the gate.
      suppressedHistoryScrollEndsRef.current += 1;
    }
  }, [isLoadingOlderMessages, isRuntimeSelected, messages]);

  // The message runtime changes independently from this shared DOM scroller.
  // Position each newly projected runtime explicitly instead of inheriting the
  // previous chat's numeric scrollTop. A growing stream is anchored to its
  // newest user turn so later deltas cannot turn a one-time bottom scroll into
  // an arbitrary midpoint.
  useLayoutEffect(() => {
    const coordinator = projectionScrollCoordinatorRef.current;

    if (!isVisible) {
      coordinator.deactivate();
      historyPaginationGate.resetIntent();
      macOSWheelGestureStartPendingRef.current = false;
      macOSPreviousWheelCancelableRef.current = null;
      if (wheelGestureEndTimeoutRef.current) {
        clearTimeout(wheelGestureEndTimeoutRef.current);
        wheelGestureEndTimeoutRef.current = null;
      }
      return;
    }

    const projectionChanged = coordinator.activate(renderedRuntimeKey, resolveScrollProjectionKey);

    if (projectionChanged) {
      clearHistoryBottomCompensation();
      historyPaginationGate.resetIntent();
      pendingHistoryScrollRestoreRef.current = null;
      pendingHistoryScrollRestoreKeyRef.current = null;
      isUserScrollingRef.current = false;
      setIsUserScrolling(false);
      prevLastMessageId.current = lastMessageId;
      prevStreamingRef.current = hasStreamingMessage;
    }

    const container = chatContainerRef.current;
    const canonicalKey = runtimeStore.resolveKey(renderedRuntimeKey);
    const isFreshDraft = isDraftChatRuntimeKey(canonicalKey);
    const projectionReady = Boolean(
      container &&
      !isLoadingOlderMessages &&
      (messages.length > 0 || activeRuntime.historyLoaded || isFreshDraft)
    );

    if (
      !container ||
      !coordinator.takePositionRequest(
        renderedRuntimeKey,
        projectionReady,
        resolveScrollProjectionKey
      )
    ) {
      return;
    }

    const target = chatProjectionScrollTarget(messages, isGenerating);

    if (target.type === "latest-user") {
      const userTurn = Array.from(
        container.querySelectorAll<HTMLElement>("[data-history-anchor-ids]")
      ).find((candidate) =>
        candidate.dataset.historyAnchorIds?.split(" ").includes(target.messageId)
      );

      if (userTurn) {
        container.scrollTop = projectedUserTurnScrollTop({
          currentScrollTop: container.scrollTop,
          containerTop: container.getBoundingClientRect().top,
          userTurnTop: userTurn.getBoundingClientRect().top
        });
        return;
      }
    }

    container.scrollTop = container.scrollHeight;
  }, [
    activeRuntime.historyLoaded,
    clearHistoryBottomCompensation,
    hasStreamingMessage,
    isGenerating,
    isLoadingOlderMessages,
    isVisible,
    lastMessageId,
    messages,
    projectionScrollCoordinatorRef,
    renderedRuntimeKey,
    resolveScrollProjectionKey,
    runtimeStore,
    historyPaginationGate
  ]);

  // Auto-scroll when user sends a message
  // Track the LAST message ID (at the end of the array), not the count
  useEffect(() => {
    // Only scroll if the LAST message changed, which means an item was added to the END
    // (not prepended while loading older history).
    if (lastMessageId !== prevLastMessageId.current && messages.length > 0) {
      const lastItem = messages[messages.length - 1];
      const isUserMessage =
        lastItem.type === "message" && (lastItem as ExtendedMessage).role === "user";
      const shouldAutoScrollAssistantItem =
        isAssistantConversationItem(lastItem) && !isUserScrolling;

      if (isUserMessage || shouldAutoScrollAssistantItem) {
        const projectionLease = projectionScrollCoordinatorRef.current.captureLease(
          renderedRuntimeKey,
          resolveScrollProjectionKey
        );
        setTimeout(
          () => {
            runForProjectedRuntime(projectionLease, () => scrollToBottom("smooth"));
          },
          isUserMessage ? 50 : 0
        );
      }
    }
    prevLastMessageId.current = lastMessageId;
  }, [
    isUserScrolling,
    lastMessageId,
    messages,
    projectionScrollCoordinatorRef,
    renderedRuntimeKey,
    resolveScrollProjectionKey,
    runForProjectedRuntime,
    scrollToBottom
  ]);

  // Auto-scroll when assistant starts streaming (but not while streaming)
  useEffect(() => {
    let timeoutId: ReturnType<typeof setTimeout> | undefined;

    if (hasStreamingMessage && !prevStreamingRef.current && !isUserScrolling) {
      // Just started streaming - scroll slightly to show the loading indicator
      const projectionLease = projectionScrollCoordinatorRef.current.captureLease(
        renderedRuntimeKey,
        resolveScrollProjectionKey
      );
      timeoutId = setTimeout(() => {
        runForProjectedRuntime(projectionLease, () => {
          const container = chatContainerRef.current;
          if (!container) return;

          // Scroll just enough to show the streaming message started
          const currentScroll = container.scrollTop;
          const maxScroll = container.scrollHeight - container.clientHeight;
          // Scroll down 100px or to bottom, whichever is less
          const targetScroll = Math.min(currentScroll + 100, maxScroll);
          container.scrollTo({
            top: targetScroll,
            behavior: "smooth"
          });
        });
      }, 100);
    }

    prevStreamingRef.current = hasStreamingMessage;

    return () => {
      if (timeoutId !== undefined) clearTimeout(timeoutId);
    };
  }, [
    hasStreamingMessage,
    isUserScrolling,
    projectionScrollCoordinatorRef,
    renderedRuntimeKey,
    resolveScrollProjectionKey,
    runForProjectedRuntime
  ]);

  // Auto-scroll when new messages arrive from polling
  useEffect(() => {
    if (newPolledMessagesOwnerKey) {
      // New messages arrived from polling - scroll to bottom to show them
      const projectionLease = projectionScrollCoordinatorRef.current.captureLease(
        newPolledMessagesOwnerKey,
        resolveScrollProjectionKey
      );
      setTimeout(() => {
        runForProjectedRuntime(projectionLease, () => scrollToBottom("smooth"));
      }, 100);

      // Reset the flag
      setNewPolledMessagesOwnerKey(null);
    }
  }, [
    newPolledMessagesOwnerKey,
    projectionScrollCoordinatorRef,
    resolveScrollProjectionKey,
    runForProjectedRuntime,
    scrollToBottom
  ]);

  const selectConversationRuntime = useCallback(
    (conversationId: string) => {
      const key = createConversationChatKey(conversationId);
      stopRecordingForNavigation(key);
      runtimeStore.select(key);
      activeRuntimeKeyRef.current = key;
      setActiveRuntimeKey(key);
      setChatId(conversationId);
      return key;
    },
    [runtimeStore, stopRecordingForNavigation]
  );

  const selectFreshDraftRuntime = useCallback(
    (projectId: string | null, requestedDraftKey?: DraftChatRuntimeKey) => {
      const key = requestedDraftKey ?? createChatDraftKey();
      const draftProjectId = draftScopeForRuntimeSelection(runtimeStore, key, projectId);
      stopRecordingForNavigation(key);
      runtimeStore.select(key, {
        composer: createChatComposerState(draftProjectId)
      });
      rememberChatDraftInScope(runtimeStore, key, draftProjectId);
      activeRuntimeKeyRef.current = key;
      setActiveRuntimeKey(key);
      setChatId(undefined);
      setSelectedProjectId(draftProjectId);
      window.history.replaceState(
        historyStateWithDraftRuntimeKey(window.history.state, key),
        "",
        window.location.href
      );
      return key;
    },
    [runtimeStore, setSelectedProjectId, stopRecordingForNavigation]
  );

  // Unified event handling for conversation changes. Selection never clears or
  // aborts the previous runtime; background streams retain their owning key.
  useEffect(() => {
    // Handle new chat event
    const handleNewChat = (event?: Event) => {
      const detail =
        event instanceof CustomEvent && event.detail && typeof event.detail === "object"
          ? (event.detail as NewChatNavigationDetail)
          : undefined;
      const nextProjectId =
        detail && "projectId" in detail ? (detail.projectId ?? null) : (selectedProjectId ?? null);
      const requestedDraftKey = isDraftChatRuntimeKey(detail?.draftRuntimeKey)
        ? detail.draftRuntimeKey
        : undefined;

      selectFreshDraftRuntime(nextProjectId, requestedDraftKey);
    };

    // Handle conversation selection from sidebar
    const handleConversationSelected = (event: CustomEvent) => {
      const { conversationId } = event.detail;
      if (conversationId && conversationId !== chatId) {
        selectConversationRuntime(conversationId);
      }
    };

    // Handle browser back/forward navigation
    const handlePopState = (event: PopStateEvent) => {
      const params = new URLSearchParams(window.location.search);
      const newChatId = params.get("conversation_id") || undefined;
      if (params.has("project_id")) return;

      if (newChatId) {
        if (newChatId !== chatId) {
          selectConversationRuntime(newChatId);
        }
        return;
      }

      const savedDraftKey = draftRuntimeKeyFromHistoryState(event.state);
      if (savedDraftKey) {
        const restoredConversationId = conversationIdFromChatRuntimeKey(
          runtimeStore.resolveKey(savedDraftKey)
        );
        if (restoredConversationId) {
          canonicalizeConversationHistoryEntry(restoredConversationId, event.state);
          if (restoredConversationId !== chatId) {
            selectConversationRuntime(restoredConversationId);
          }
          return;
        }
      }

      const currentKey = runtimeStore.resolveKey(activeRuntimeKeyRef.current);
      if (!savedDraftKey || currentKey !== runtimeStore.resolveKey(savedDraftKey)) {
        selectFreshDraftRuntime(selectedProjectId ?? null, savedDraftKey ?? undefined);
      }
    };

    window.addEventListener("newchat", handleNewChat);
    window.addEventListener("conversationselected", handleConversationSelected as EventListener);
    window.addEventListener("popstate", handlePopState);

    return () => {
      window.removeEventListener("newchat", handleNewChat);
      window.removeEventListener(
        "conversationselected",
        handleConversationSelected as EventListener
      );
      window.removeEventListener("popstate", handlePopState);
    };
  }, [chatId, runtimeStore, selectConversationRuntime, selectFreshDraftRuntime, selectedProjectId]);

  const settleLocallyCancelledRun = useCallback(
    (runtimeKey: ChatRuntimeKey, runToken: number, optimisticMessageId: string | undefined) => {
      const cancelled = runtimeStore.cancelRun(runtimeKey, runToken);
      if (!cancelled) return false;
      clearUnresolvedChatResponseMessage(runtimeStore, runToken);
      runtimeStore.update(runtimeKey, (snapshot) => ({
        ...snapshot,
        error: snapshot.error === CHAT_STOP_WAITING_MESSAGE ? null : snapshot.error,
        messages: cancelled.responseId
          ? updateActiveItemStatuses(snapshot.messages as Message[], "incomplete")
          : updateActiveItemStatuses(
              markOptimisticMessageIncomplete(snapshot.messages as Message[], optimisticMessageId),
              "incomplete"
            )
      }));
      if (optimisticMessageId) {
        unregisterChatOptimisticMessage(runtimeStore, runToken, optimisticMessageId);
      }
      return true;
    },
    [runtimeStore]
  );

  const cancelKnownChatResponse = useCallback(
    async (
      runtimeKey: ChatRuntimeKey,
      runToken: number,
      responseId: string,
      optimisticMessageId: string | undefined
    ): Promise<boolean> => {
      if (!openai) return false;
      try {
        await (
          openai.responses as {
            cancel: (id: string, options?: { timeout?: number }) => Promise<unknown>;
          }
        ).cancel(responseId, { timeout: CHAT_STOP_REQUEST_TIMEOUT_MS });
        return settleLocallyCancelledRun(runtimeKey, runToken, optimisticMessageId);
      } catch (error) {
        console.error("Failed to cancel response:", error);
        if (!isChatResponseCancellationAlreadyTerminalError(error)) {
          if (runtimeStore.isRunCurrent(runtimeKey, runToken)) {
            setErrorForKey(runtimeKey, "Failed to cancel response. Please try Stop again.");
          }
          return false;
        }
        try {
          const response = await openai.responses.retrieve(responseId, undefined, {
            timeout: CHAT_STOP_REQUEST_TIMEOUT_MS
          });
          const status = (response as { status?: string }).status;
          const runIsCurrent = runtimeStore.isRunCurrent(runtimeKey, runToken);

          if (status === "completed") {
            // Stop intent is sticky across this reconciliation. Settle now even
            // if a buffered stream is healthy so the outer loop cannot promote
            // a queued turn after this helper releases the Stop UI fence. The
            // persisted user/item cursor remains valid for ascending polling.
            if (runIsCurrent) {
              runtimeStore.completeRunAndAbort(runtimeKey, runToken, (owned) => ({
                ...owned,
                messages: updateActiveItemStatuses(owned.messages as Message[], "completed"),
                error: "The response finished before it could be stopped."
              }));
              clearUnresolvedChatResponseMessage(runtimeStore, runToken);
              return true;
            }
            return false;
          }

          if (status === "failed" || status === "cancelled" || status === "incomplete") {
            return settleLocallyCancelledRun(runtimeKey, runToken, optimisticMessageId);
          }
        } catch (reconciliationError) {
          console.error(
            "Failed to reconcile response after cancellation error:",
            reconciliationError
          );
        }

        if (runtimeStore.isRunCurrent(runtimeKey, runToken)) {
          setErrorForKey(runtimeKey, "Failed to cancel response. Please try Stop again.");
        }
        return false;
      } finally {
        // Keep this fence through cancellation/retrieval so the send loop
        // cannot promote a later FIFO item under the same outer run token.
        stoppingRuntimeRegistry.delete(runtimeKey, runToken);
        if (!runtimeStore.isRunCurrent(runtimeKey, runToken)) {
          clearChatRunQueueHalt(runtimeStore, runToken);
        }
      }
    },
    [openai, runtimeStore, setErrorForKey, settleLocallyCancelledRun, stoppingRuntimeRegistry]
  );

  const reconcileDetachedChatResponse = useCallback(
    async (runtimeKey: ChatRuntimeKey) => {
      const snapshot = runtimeStore.get(runtimeKey);
      if (
        !openai ||
        !snapshot?.isGenerating ||
        snapshot.runToken === null ||
        !snapshot.currentResponseId
      ) {
        return;
      }

      const runToken = snapshot.runToken;
      const responseId = snapshot.currentResponseId;
      const reconciliationKey = `${runtimeStore.resolveKey(runtimeKey)}:${runToken}:${responseId}`;
      if (responseReconciliationsInFlightRef.current.has(reconciliationKey)) return;
      responseReconciliationsInFlightRef.current.add(reconciliationKey);

      try {
        if (isChatRunQueueHaltRequested(runtimeStore, runToken)) {
          await cancelKnownChatResponse(runtimeKey, runToken, responseId, undefined);
          return;
        }
        const response = await openai.responses.retrieve(responseId, undefined, {
          timeout: CHAT_STOP_REQUEST_TIMEOUT_MS
        });
        const current = runtimeStore.get(runtimeKey);
        if (!current || current.runToken !== runToken || current.currentResponseId !== responseId) {
          return;
        }

        const resolution = classifyChatResponseReconciliation(
          (response as { status?: string | null }).status
        );
        if (resolution === "completed") {
          runtimeStore.completeRunAndAbort(runtimeKey, runToken, (owned) => ({
            ...owned,
            messages: updateActiveItemStatuses(owned.messages as Message[], "completed"),
            error:
              "The response completed after the connection was restored. Queued messages were kept."
          }));
          clearUnresolvedChatResponseMessage(runtimeStore, runToken);
          stoppingRuntimeRegistry.delete(runtimeKey, runToken);
          clearChatRunQueueHalt(runtimeStore, runToken);
        } else if (resolution === "terminal") {
          if (settleLocallyCancelledRun(runtimeKey, runToken, undefined)) {
            setErrorForKey(
              runtimeKey,
              "The interrupted response stopped. Queued messages were kept."
            );
            stoppingRuntimeRegistry.delete(runtimeKey, runToken);
            clearChatRunQueueHalt(runtimeStore, runToken);
          }
        }
      } catch (error) {
        console.error("Failed to reconcile interrupted response:", error);
      } finally {
        responseReconciliationsInFlightRef.current.delete(reconciliationKey);
      }
    },
    [
      cancelKnownChatResponse,
      openai,
      runtimeStore,
      setErrorForKey,
      settleLocallyCancelledRun,
      stoppingRuntimeRegistry
    ]
  );

  // Cancel the current response
  const handleCancelResponse = useCallback(async () => {
    const runtimeKey = runtimeStore.resolveKey(activeRuntimeKeyRef.current);
    const runSnapshot = runtimeStore.get(runtimeKey);
    if (!runSnapshot || runSnapshot.runToken === null) return;
    const runToken = runSnapshot.runToken;
    stoppingRuntimeRegistry.add(runtimeKey, runToken);
    requestChatRunQueueHalt(runtimeStore, runToken);
    const optimisticMessageId = getRegisteredChatOptimisticMessage(runtimeStore, runToken);
    const restoredBeforeRequest = restoreRegisteredChatTurnBeforeRequest(
      runtimeStore,
      runToken,
      "Stopped before sending. Your message was restored."
    );
    // Commit the final partial frame while this run still owns its token. Once
    // cancelRun clears ownership, any delayed callback must fail closed.
    flushRegisteredChatStreamDeltas(runtimeStore, runToken);

    if (restoredBeforeRequest) {
      settleLocallyCancelledRun(runtimeKey, runToken, optimisticMessageId);
      stoppingRuntimeRegistry.delete(runtimeKey, runToken);
      return;
    }

    const currentResponseId = runtimeStore.get(runtimeKey)?.currentResponseId;
    if (currentResponseId) {
      await cancelKnownChatResponse(runtimeKey, runToken, currentResponseId, optimisticMessageId);
      return;
    }

    // The POST may already be accepted even though response.created has not
    // arrived. Preserve the run and Stop fence; the stream handler will cancel
    // immediately when it learns the server response ID. Local cancellation
    // here would orphan server work and allow the FIFO to overlap it.
    setErrorForKey(runtimeKey, CHAT_STOP_WAITING_MESSAGE);
  }, [
    cancelKnownChatResponse,
    runtimeStore,
    setErrorForKey,
    settleLocallyCancelledRun,
    stoppingRuntimeRegistry
  ]);

  // Load conversation from API
  const loadConversation = useCallback(
    async (runtimeKey: ChatRuntimeKey, conversationId: string) => {
      if (!openai) return;

      const canonicalKey = runtimeStore.resolveKey(runtimeKey);
      const requestId = (activeConversationLoadRef.current.get(canonicalKey) ?? 0) + 1;
      activeConversationLoadRef.current.set(canonicalKey, requestId);
      const isStaleRequest = () =>
        !runtimeStore.get(runtimeKey) ||
        activeConversationLoadRef.current.get(runtimeStore.resolveKey(runtimeKey)) !== requestId;

      try {
        // Start both fetches immediately in parallel
        const convPromise = openai.conversations.retrieve(conversationId).then(
          (conv) => ({ ok: true as const, value: conv }),
          (error) => ({ ok: false as const, error })
        );
        const itemsPromise = openai.conversations.items.list(conversationId, {
          limit: 20,
          order: "desc"
        });

        // Process items as soon as they're ready (don't wait for metadata)
        const itemsResponse = await itemsPromise;
        if (isStaleRequest()) return;

        // Convert items to messages, grouping tool calls with their messages
        const loadedMessages = convertItemsToMessages(
          itemsResponse.data as Array<{
            id: string;
            type: string;
            role?: string;
            content?: unknown;
            name?: string;
            arguments?: string;
            call_id?: string;
            output?: string;
            status?: string;
            created_at?: number;
          }>
        );

        // Reverse the array for display (we want oldest first/at top, newest last/at bottom)
        // API returns desc (newest first), but chat UI needs chronological order
        const messagesInChronologicalOrder = loadedMessages.reverse();
        const oldestId = itemsResponse.last_id || itemsResponse.data.at(-1)?.id;
        const newestCompletedItem = itemsResponse.data.find(
          (item) => (item as Message).status !== "in_progress"
        );
        const snapshotBeforeItems = runtimeStore.get(runtimeKey);
        if (
          snapshotBeforeItems?.isGenerating &&
          !snapshotBeforeItems.currentResponseId &&
          snapshotBeforeItems.runToken !== null
        ) {
          const unresolvedMessageId = getUnresolvedChatResponseMessage(
            runtimeStore,
            snapshotBeforeItems.runToken
          );
          const recoveredResponseId = unresolvedMessageId
            ? responseIdForChatMessage(unresolvedMessageId, itemsResponse.data)
            : undefined;
          if (recoveredResponseId) {
            runtimeStore.setCurrentResponseId(
              runtimeKey,
              snapshotBeforeItems.runToken,
              recoveredResponseId
            );
            clearUnresolvedChatResponseMessage(
              runtimeStore,
              snapshotBeforeItems.runToken,
              unresolvedMessageId
            );
          }
        }
        runtimeStore.update(runtimeKey, (snapshot) => ({
          ...snapshot,
          messages: mergeLoadedMessagesWithRuntime(messagesInChronologicalOrder, snapshot.messages),
          lastSeenItemId: newestCompletedItem?.id ?? snapshot.lastSeenItemId,
          historyLoaded: true,
          composer: {
            ...snapshot.composer,
            pagination: {
              ...snapshot.composer.pagination,
              oldestItemId: oldestId,
              hasMoreOlderMessages: oldestId ? itemsResponse.has_more : false,
              isLoadingOlderMessages: false
            }
          }
        }));

        // Then handle conversation metadata when it arrives
        const conv = await convPromise;
        if (isStaleRequest()) return;
        if (!conv.ok) {
          throw conv.error;
        }
        const conversation = conv.value as Conversation;
        runtimeStore.update(runtimeKey, (snapshot) => ({
          ...snapshot,
          conversation
        }));
        runtimeStore.updateActivityGroup(runtimeKey, conversation.project_id ?? null);
      } catch (error) {
        if (isStaleRequest()) return;

        const err = error as { status?: number; message?: string };
        if (err.status === 404) {
          // Conversation doesn't exist - clear and start fresh
          // Conversation not found, starting new
          const projectedRuntimeWasDeleted =
            runtimeStore.resolveKey(activeRuntimeKeyRef.current) ===
            runtimeStore.resolveKey(runtimeKey);
          if (projectedRuntimeWasDeleted) {
            if (isRuntimeSelected(runtimeKey)) {
              const params = new URLSearchParams(window.location.search);
              params.delete("conversation_id");
              window.history.replaceState({}, "", params.toString() ? `/?${params}` : "/");
            }
            selectFreshDraftRuntime(selectedProjectId ?? null);
          }
          runtimeStore.delete(runtimeKey);
        } else {
          console.error("Failed to load conversation:", error);
          setErrorForKey(runtimeKey, err.message || "Failed to load conversation");
        }
      }
    },
    [
      activeConversationLoadRef,
      isRuntimeSelected,
      openai,
      runtimeStore,
      selectFreshDraftRuntime,
      selectedProjectId,
      setErrorForKey
    ]
  );

  // Load older messages for pagination
  const loadOlderMessages = useCallback(async () => {
    const { gate: paginationGate, runtimeKey } = historyPaginationLifecycle;
    const snapshot = runtimeStore.get(runtimeKey);
    const conversationId =
      snapshot?.conversation?.id ??
      conversationIdFromChatRuntimeKey(runtimeStore.resolveKey(runtimeKey));
    const requestOldestItemId = snapshot?.composer.pagination.oldestItemId;
    if (!conversationId || !openai || !requestOldestItemId) {
      paginationGate.finishLoad();
      return;
    }

    // Preserve the last stable viewport before the request yields. A fast
    // upward gesture can elastically move every message out of WKWebView's
    // viewport while the network request is pending, leaving nothing reliable
    // to anchor when the response arrives.
    const projectionLease = projectionScrollCoordinatorRef.current.captureLease(
      runtimeKey,
      resolveScrollProjectionKey
    );
    const requestStartScrollSnapshot = captureHistoryScrollSnapshot(runtimeKey);

    updateComposerForKey(runtimeKey, (composer) => ({
      ...composer,
      pagination: { ...composer.pagination, isLoadingOlderMessages: true }
    }));

    let pageProgressed = false;

    try {
      // Fetch next 20 older items using the oldest item ID we have
      const itemsResponse = await openai.conversations.items.list(conversationId, {
        limit: 20,
        order: "desc",
        after: requestOldestItemId
      });

      // Convert items to messages, grouping tool calls with their messages
      const olderMessages = convertItemsToMessages(
        itemsResponse.data as Array<{
          id: string;
          type: string;
          role?: string;
          content?: unknown;
          name?: string;
          arguments?: string;
          call_id?: string;
          output?: string;
          status?: string;
          created_at?: number;
        }>
      );

      // Reverse for chronological order (API returns desc, we need asc for display).
      const olderMessagesInChronologicalOrder = olderMessages.reverse();
      const newOldestId = itemsResponse.last_id || itemsResponse.data.at(-1)?.id;

      if (!chatHistoryCursorProgressed(requestOldestItemId, newOldestId)) {
        updateComposerForKey(runtimeKey, (composer) => ({
          ...composer,
          pagination: { ...composer.pagination, hasMoreOlderMessages: false }
        }));
        return;
      }

      if (olderMessagesInChronologicalOrder.length > 0) {
        const stillOwnsStartingProjection = Boolean(
          projectionLease &&
          projectionScrollCoordinatorRef.current.ownsLease(
            projectionLease,
            resolveScrollProjectionKey
          )
        );
        if (stillOwnsStartingProjection) {
          const scrollSnapshot = preferredChatHistoryScrollSnapshot({
            requestStartSnapshot: requestStartScrollSnapshot,
            commitSnapshot: captureHistoryScrollSnapshot(runtimeKey)
          });
          if (scrollSnapshot) {
            pendingHistoryScrollRestoreRef.current = scrollSnapshot;
            pendingHistoryScrollRestoreKeyRef.current = runtimeKey;
          }
        }

        // Prepend older messages while keeping IDs unique. Restore the pending scroll
        // snapshot after this render commits; the loading overlay does not affect layout.
        runtimeStore.update(runtimeKey, (current) => ({
          ...current,
          messages: mergeMessagesById(
            olderMessagesInChronologicalOrder,
            current.messages as Message[]
          ),
          composer: {
            ...current.composer,
            pagination: {
              ...current.composer.pagination,
              oldestItemId: newOldestId,
              hasMoreOlderMessages: itemsResponse.has_more
            }
          }
        }));
        pageProgressed = true;
      } else {
        pageProgressed = updateComposerForKey(runtimeKey, (composer) => ({
          ...composer,
          pagination: {
            ...composer.pagination,
            oldestItemId: newOldestId,
            hasMoreOlderMessages: itemsResponse.has_more
          }
        }));
      }
    } catch (error) {
      console.error("Failed to load older messages:", error);
    } finally {
      paginationGate.finishLoad({ preserveQueuedLoad: pageProgressed });
      const current = runtimeStore.get(runtimeKey);
      if (current) {
        updateComposerForKey(runtimeKey, (composer) => ({
          ...composer,
          pagination: { ...composer.pagination, isLoadingOlderMessages: false }
        }));
      }
    }
  }, [
    captureHistoryScrollSnapshot,
    historyPaginationLifecycle,
    openai,
    projectionScrollCoordinatorRef,
    resolveScrollProjectionKey,
    runtimeStore,
    updateComposerForKey
  ]);

  // Polling mechanism for conversation updates
  const pollForNewItems = useCallback(
    async (runtimeKey: ChatRuntimeKey) => {
      const snapshot = runtimeStore.get(runtimeKey);
      const conversationId =
        snapshot?.conversation?.id ??
        conversationIdFromChatRuntimeKey(runtimeStore.resolveKey(runtimeKey));
      if (
        !snapshot ||
        !conversationId ||
        !openai ||
        (snapshot.assistantStreaming && snapshot.currentResponseId)
      ) {
        return;
      }

      try {
        if (!snapshot.currentResponseId && snapshot.runToken !== null) {
          const unresolvedMessageId = getUnresolvedChatResponseMessage(
            runtimeStore,
            snapshot.runToken
          );
          // Retrieve the exact persisted user item. A list page can contain ten
          // image-description call/output pairs before the user item and is not
          // a reliable response-ownership lookup.
          if (unresolvedMessageId) {
            try {
              const linkedItem = await openai.conversations.items.retrieve(unresolvedMessageId, {
                conversation_id: conversationId
              });
              const recoveredResponseId = responseIdForChatMessage(unresolvedMessageId, [
                linkedItem
              ]);
              if (
                recoveredResponseId &&
                runtimeStore.setCurrentResponseId(
                  runtimeKey,
                  snapshot.runToken,
                  recoveredResponseId
                )
              ) {
                clearUnresolvedChatResponseMessage(
                  runtimeStore,
                  snapshot.runToken,
                  unresolvedMessageId
                );
              }
            } catch (error) {
              const status = (error as { status?: unknown })?.status;
              if (status !== 404) console.error("Response ownership polling error:", error);
            }
          }
        }

        // Fetch NEW items that came after the last seen ID
        // Use order=asc to get items chronologically after the lastSeenItemId
        const hasCursor = Boolean(snapshot.lastSeenItemId);
        const response = await openai.conversations.items.list(conversationId, {
          ...(snapshot.lastSeenItemId ? { after: snapshot.lastSeenItemId } : {}),
          order: hasCursor ? "asc" : "desc",
          limit: 20 // Smaller limit since we only expect a few new messages
        });
        const pollingPage = normalizeChatPollingPage(response.data, hasCursor);

        if (!snapshot.currentResponseId && snapshot.runToken !== null) {
          const unresolvedMessageId = getUnresolvedChatResponseMessage(
            runtimeStore,
            snapshot.runToken
          );
          const recoveredResponseId = unresolvedMessageId
            ? responseIdForChatMessage(unresolvedMessageId, response.data)
            : undefined;
          if (recoveredResponseId) {
            if (
              runtimeStore.setCurrentResponseId(runtimeKey, snapshot.runToken, recoveredResponseId)
            ) {
              clearUnresolvedChatResponseMessage(
                runtimeStore,
                snapshot.runToken,
                unresolvedMessageId
              );
            }
          }
        }

        if (response.data.length > 0) {
          // Convert API items to UI messages, grouping tool calls with their messages
          const newMessages = convertItemsToMessages(
            pollingPage.chronologicalItems as Array<{
              id: string;
              type: string;
              role?: string;
              content?: unknown;
              name?: string;
              arguments?: string;
              call_id?: string;
              output?: string;
              status?: string;
              created_at?: number;
            }>
          );

          if (newMessages.length > 0) {
            // Merge new messages with deduplication using helper
            runtimeStore.update(runtimeKey, (current) => {
              // Check if there are truly new messages (not already in prev)
              const prevIds = new Set(current.messages.map((m) => m.id));
              const trulyNewMessages = newMessages.filter((m) => !prevIds.has(m.id));

              // Mark that we have new polled messages for scrolling
              if (trulyNewMessages.length > 0 && isRuntimeSelected(runtimeKey)) {
                setNewPolledMessagesOwnerKey(runtimeKey);
              }

              return {
                ...current,
                messages: mergePolledMessagesWithRuntime(current.messages, newMessages)
              };
            });

            // Update last seen item ID for next poll
            // Since we're using order=asc, the LAST item is the newest
            // Skip in_progress messages by finding the last completed one
            const newestCompletedItem = pollingPage.newestCompletedItem;
            if (newestCompletedItem?.id) {
              setLastSeenItemIdForKey(runtimeKey, newestCompletedItem.id);
            }
          }
        }
      } catch (error) {
        console.error("Polling error:", error);
        // Don't throw - polling should fail silently
      }
      if (!runtimeStore.get(runtimeKey)?.conversation) {
        try {
          const conversation = (await openai.conversations.retrieve(
            conversationId
          )) as Conversation;
          if (runtimeStore.get(runtimeKey)) {
            runtimeStore.update(runtimeKey, (current) => ({ ...current, conversation }));
            runtimeStore.updateActivityGroup(runtimeKey, conversation.project_id ?? null);
          }
        } catch (error) {
          console.error("Conversation metadata polling error:", error);
        }
      }
      await reconcileDetachedChatResponse(runtimeKey);
    },
    [
      isRuntimeSelected,
      openai,
      reconcileDetachedChatResponse,
      runtimeStore,
      setLastSeenItemIdForKey
    ]
  );

  // Load conversation when URL changes or on mount. Cached runtimes—including
  // active offscreen streams—remain authoritative and do not get reloaded.
  useEffect(() => {
    if (chatId && openai) {
      const snapshot = runtimeStore.get(activeRuntimeKey);
      if (!snapshot?.historyLoaded) {
        void loadConversation(activeRuntimeKey, chatId);
      }
    }
  }, [activeRuntimeKey, chatId, openai, loadConversation, runtimeStore]);

  // Set up progressive polling interval
  const pollingConversationId =
    conversation?.id ?? conversationIdFromChatRuntimeKey(runtimeStore.resolveKey(activeRuntimeKey));

  useEffect(() => {
    if (!pollingConversationId || !openai) return;
    const runtimeKey = activeRuntimeKey;

    // Progressive intervals: 2s, 5s, 10s, 15s, 20s, 30s, 60s (then 60s forever)
    const intervals = [2000, 5000, 10000, 15000, 20000, 30000, 60000];
    let currentIntervalIndex = 0;
    let timeoutId: ReturnType<typeof setTimeout>;

    const scheduleNextPoll = () => {
      // Get current interval (use last interval if we've reached the end)
      const currentInterval = intervals[Math.min(currentIntervalIndex, intervals.length - 1)];

      timeoutId = setTimeout(() => {
        void pollForNewItems(runtimeKey);

        // Move to next interval if not at the end
        if (currentIntervalIndex < intervals.length - 1) {
          currentIntervalIndex++;
        }

        // Schedule the next poll
        scheduleNextPoll();
      }, currentInterval);
    };

    // Start the progressive polling (first poll after 2s)
    scheduleNextPoll();

    return () => {
      if (timeoutId) clearTimeout(timeoutId);
    };
  }, [activeRuntimeKey, openai, pollForNewItems, pollingConversationId]);

  // Poll for title updates when it's "New Conversation" with exponential backoff
  useEffect(() => {
    const currentTitle = conversation?.metadata?.title;
    const runtimeKey = activeRuntimeKey;

    // Only poll if we have a conversation and the title is "New Conversation"
    if (!conversation?.id || !openai || currentTitle !== "New Conversation") {
      return;
    }

    // Exponential backoff: 0.5s, 1s, 2s, 4s, 8s, 10s (max)
    let currentDelay = 500; // Start at 0.5s
    const maxDelay = 10000; // Cap at 10s
    let timeoutId: ReturnType<typeof setTimeout>;
    let cancelled = false;

    const checkTitle = async () => {
      try {
        // Fetch updated conversation metadata
        const updatedConv = await openai.conversations.retrieve(conversation.id);
        if (cancelled || !runtimeStore.get(runtimeKey)) return;
        const newTitle = (updatedConv as Conversation).metadata?.title;

        // If title changed from "New Conversation", update local state and sidebar
        if (newTitle && newTitle !== "New Conversation") {
          setConversationForKey(runtimeKey, updatedConv as Conversation);
          // Trigger title animation
          if (isRuntimeSelected(runtimeKey)) setTitleJustUpdatedKey(runtimeKey);
          // Remove animation class after animation completes (800ms for flash animation)
          setTimeout(() => {
            setTitleJustUpdatedKey((currentKey) =>
              currentKey &&
              runtimeStore.resolveKey(currentKey) === runtimeStore.resolveKey(runtimeKey)
                ? null
                : currentKey
            );
          }, 850);
          // Refresh all sidebar conversation lists
          await Promise.all([
            queryClient.invalidateQueries({ queryKey: ["conversations"] }),
            queryClient.invalidateQueries({ queryKey: ["pinnedConversations"] }),
            queryClient.invalidateQueries({ queryKey: ["projectConversations"] })
          ]);
          return; // Stop polling once title is updated
        }

        // Schedule next check with exponential backoff
        currentDelay = Math.min(currentDelay * 2, maxDelay);
        timeoutId = setTimeout(checkTitle, currentDelay);
      } catch (error) {
        console.error("Failed to check title update:", error);
        if (cancelled) return;
        // Continue polling even on error
        currentDelay = Math.min(currentDelay * 2, maxDelay);
        timeoutId = setTimeout(checkTitle, currentDelay);
      }
    };

    // Start the first check after 0.5s
    timeoutId = setTimeout(checkTitle, currentDelay);

    return () => {
      cancelled = true;
      if (timeoutId) clearTimeout(timeoutId);
    };
  }, [
    activeRuntimeKey,
    conversation?.id,
    conversation?.metadata?.title,
    isRuntimeSelected,
    openai,
    queryClient,
    runtimeStore,
    setConversationForKey
  ]);

  const isHistoryTopBoundaryNear = useCallback(() => {
    const container = chatContainerRef.current;
    const sentinel = historyTopSentinelRef.current;
    if (!container || !sentinel) return false;

    const containerRect = container.getBoundingClientRect();
    const sentinelRect = sentinel.getBoundingClientRect();
    if (container.scrollHeight <= container.clientHeight + 1) return true;

    return (
      sentinelRect.bottom >= containerRect.top - CHAT_HISTORY_TOP_MARGIN_PX &&
      sentinelRect.top <= containerRect.top + CHAT_HISTORY_TOP_MARGIN_PX
    );
  }, []);

  const maybeLoadOlderMessages = useCallback(() => {
    const canLoad = Boolean(hasMoreOlderMessages && conversation?.id && openai && oldestItemId);
    const shouldLoad = historyPaginationGate.tryStartLoad({
      canLoad,
      topBoundaryVisible: isHistoryTopBoundaryNear(),
      requestInFlight: isLoadingOlderMessages
    });

    if (shouldLoad) {
      void loadOlderMessages();
    }
  }, [
    conversation?.id,
    hasMoreOlderMessages,
    historyPaginationGate,
    isHistoryTopBoundaryNear,
    isLoadingOlderMessages,
    loadOlderMessages,
    oldestItemId,
    openai
  ]);

  // A second physical wheel gesture can reach the old boundary while the
  // previous page is still in flight. Once that page has committed and its
  // visible anchor has been restored, honor the one bounded request already
  // made at that boundary. This effect runs after the restoration layout
  // effect above, so consecutive prepends never share an uncommitted anchor.
  useLayoutEffect(() => {
    if (isLoadingOlderMessages) return;

    const canLoad = Boolean(hasMoreOlderMessages && conversation?.id && openai && oldestItemId);
    if (historyPaginationGate.tryStartQueuedLoad({ canLoad })) {
      void loadOlderMessages();
    }
  }, [
    conversation?.id,
    hasMoreOlderMessages,
    historyPaginationGate,
    isLoadingOlderMessages,
    loadOlderMessages,
    oldestItemId,
    openai
  ]);

  // Only direct backward-navigation input can arm history pagination. Intersection,
  // resize, initial positioning, and card expansion merely update boundary visibility.
  useLayoutEffect(() => {
    const container = chatContainerRef.current;
    if (!container || !isVisible) return;

    const gate = historyPaginationGate;
    const usesMacOSWheelGestureStart = usesFirstCancelableWheelGestureStart({
      isTauriEnvironment: isTauriEnv,
      browserPlatform: navigator.platform
    });
    const delaysWheelGestureEndAfterScrollEnd = isTauriEnv && isMacOS();

    const finishWheelGesture = () => {
      macOSWheelGestureStartPendingRef.current = false;
      macOSPreviousWheelCancelableRef.current = null;
      gate.endGesture();
      wheelGestureEndTimeoutRef.current = null;
    };

    const finishTouchGesture = () => {
      touchHistoryGestureActiveRef.current = false;
      gate.endGesture();
      touchGestureEndTimeoutRef.current = null;
    };

    const handleHistoryScrollEnd = () => {
      // `scrollend` follows the browser's real wheel/trackpad and keyboard gesture
      // boundary, including momentum. Keep the timers below as a compatibility
      // fallback for WebViews that do not dispatch it.
      if (suppressedHistoryScrollEndsRef.current > 0) {
        suppressedHistoryScrollEndsRef.current -= 1;
        return;
      }
      if (pointerHistoryGestureActiveRef.current) return;
      if (touchHistoryGestureActiveRef.current) {
        if (touchGestureEndTimeoutRef.current) {
          clearTimeout(touchGestureEndTimeoutRef.current);
        }
        finishTouchGesture();
        return;
      }

      if (wheelGestureEndTimeoutRef.current) {
        clearTimeout(wheelGestureEndTimeoutRef.current);
        if (delaysWheelGestureEndAfterScrollEnd) {
          // WKWebView can also emit `scrollend` between the direct and momentum
          // phases, or while an anchor restoration is settling. Give only that
          // WebView path one short quiet period; any residual event cancels this
          // release before it can re-arm pagination.
          wheelGestureEndTimeoutRef.current = setTimeout(finishWheelGesture, 80);
        } else {
          // Browsers such as Chrome expose `scrollend` as the completed wheel or
          // trackpad boundary. End immediately so a rapid follow-up gesture can
          // arm (or queue) the next page before its first wheel event is lost.
          finishWheelGesture();
        }
        return;
      }
      if (keyIntentTimeoutRef.current) {
        clearTimeout(keyIntentTimeoutRef.current);
        keyIntentTimeoutRef.current = null;
      }
      gate.endGesture();
    };

    const scheduleTouchGestureEnd = () => {
      if (touchGestureEndTimeoutRef.current) {
        clearTimeout(touchGestureEndTimeoutRef.current);
      }
      touchGestureEndTimeoutRef.current = setTimeout(finishTouchGesture, 250);
    };

    const handleHistoryWheel = (event: WheelEvent) => {
      // Chrome represents trackpad pinch-to-zoom as Ctrl+wheel. It is not
      // backward-navigation intent and must not arm or queue history loading.
      if (event.ctrlKey) return;

      const startsMacOSWheelGesture =
        usesMacOSWheelGestureStart &&
        event.cancelable &&
        macOSPreviousWheelCancelableRef.current !== true;
      if (usesMacOSWheelGestureStart) {
        macOSPreviousWheelCancelableRef.current = event.cancelable;
      }

      if (usesMacOSWheelGestureStart && event.deltaY === 0) {
        // A trackpad may expose its zero-delta MayBegin event as the first
        // cancelable event. Carry that start marker to the first directional
        // event without treating a finger touch as backward-navigation intent.
        if (startsMacOSWheelGesture) {
          macOSWheelGestureStartPendingRef.current = true;
        }
        if (wheelGestureEndTimeoutRef.current) {
          clearTimeout(wheelGestureEndTimeoutRef.current);
        }
        wheelGestureEndTimeoutRef.current = setTimeout(finishWheelGesture, 180);
        return;
      }

      if (event.deltaY >= 0) {
        if (event.deltaY > 0) {
          macOSWheelGestureStartPendingRef.current = false;
          macOSPreviousWheelCancelableRef.current = null;
          gate.endGesture();
          clearHistoryBottomCompensation();
          if (wheelGestureEndTimeoutRef.current) {
            clearTimeout(wheelGestureEndTimeoutRef.current);
            wheelGestureEndTimeoutRef.current = null;
          }
        }
        return;
      }

      if (usesMacOSWheelGestureStart) {
        const isNewWheelGesture =
          startsMacOSWheelGesture || macOSWheelGestureStartPendingRef.current;
        macOSWheelGestureStartPendingRef.current = false;
        gate.beginWheelGesture(isNewWheelGesture);
      } else {
        gate.beginGesture();
      }
      // The non-passive macOS boundary probe must stay synchronous, but avoid
      // forcing sentinel geometry while the user is still far from history's
      // top. The observer will preserve the armed intent if one large wheel
      // event crosses directly into this margin.
      if (container.scrollTop <= CHAT_HISTORY_TOP_MARGIN_PX) {
        maybeLoadOlderMessages();
      }

      if (wheelGestureEndTimeoutRef.current) {
        clearTimeout(wheelGestureEndTimeoutRef.current);
      }
      wheelGestureEndTimeoutRef.current = setTimeout(finishWheelGesture, 180);
    };

    const handleHistoryTouchStart = (event: TouchEvent) => {
      if (touchGestureEndTimeoutRef.current) {
        clearTimeout(touchGestureEndTimeoutRef.current);
        touchGestureEndTimeoutRef.current = null;
      }
      touchHistoryGestureActiveRef.current = true;
      previousTouchYRef.current = event.touches[0]?.clientY ?? null;
      gate.endGesture();
    };

    const handleHistoryTouchMove = (event: TouchEvent) => {
      const nextTouchY = event.touches[0]?.clientY;
      const previousTouchY = previousTouchYRef.current;
      if (nextTouchY === undefined || previousTouchY === null) return;

      const deltaY = nextTouchY - previousTouchY;
      previousTouchYRef.current = nextTouchY;

      if (deltaY > 2) {
        gate.beginGesture();
        maybeLoadOlderMessages();
      } else if (deltaY < -2) {
        gate.endGesture();
        clearHistoryBottomCompensation();
      }
    };

    const handleHistoryTouchEnd = () => {
      previousTouchYRef.current = null;
      maybeLoadOlderMessages();
      scheduleTouchGestureEnd();
    };

    const handleHistoryTouchCancel = () => {
      previousTouchYRef.current = null;
      finishTouchGesture();
    };

    const handleHistoryPointerDown = (event: PointerEvent) => {
      if (event.pointerType !== "mouse" || !event.isPrimary || event.button !== 0) return;

      if (wheelGestureEndTimeoutRef.current) {
        clearTimeout(wheelGestureEndTimeoutRef.current);
        wheelGestureEndTimeoutRef.current = null;
      }
      gate.endGesture();
      pointerHistoryGestureActiveRef.current = true;
      previousPointerScrollTopRef.current = container.scrollTop;
    };

    const handleHistoryPointerEnd = () => {
      if (!pointerHistoryGestureActiveRef.current) return;

      pointerHistoryGestureActiveRef.current = false;
      gate.endGesture();
    };

    const handleHistoryScroll = () => {
      const nextScrollTop = container.scrollTop;

      if (
        pointerHistoryGestureActiveRef.current &&
        nextScrollTop < previousPointerScrollTopRef.current
      ) {
        gate.beginGesture();
        maybeLoadOlderMessages();
      } else if (
        (pointerHistoryGestureActiveRef.current ||
          (touchHistoryGestureActiveRef.current && previousTouchYRef.current === null)) &&
        nextScrollTop > previousPointerScrollTopRef.current
      ) {
        clearHistoryBottomCompensation();
      }
      previousPointerScrollTopRef.current = nextScrollTop;

      // Keep touch intent alive until inertial scrolling has actually settled.
      if (touchHistoryGestureActiveRef.current && previousTouchYRef.current === null) {
        maybeLoadOlderMessages();
        scheduleTouchGestureEnd();
      }
    };

    const isBackwardHistoryKey = (event: KeyboardEvent) =>
      event.key === "ArrowUp" ||
      event.key === "PageUp" ||
      event.key === "Home" ||
      (event.shiftKey && (event.key === " " || event.key === "Spacebar"));

    const isForwardHistoryKey = (event: KeyboardEvent) =>
      event.key === "ArrowDown" ||
      event.key === "PageDown" ||
      event.key === "End" ||
      (!event.shiftKey && (event.key === " " || event.key === "Spacebar"));

    const handleHistoryKeyDown = (event: KeyboardEvent) => {
      const target = event.target;
      if (
        target instanceof Element &&
        target.closest(
          "input, textarea, select, button, a, [contenteditable='true'], [role='button'], [role='textbox']"
        )
      ) {
        return;
      }

      if (isForwardHistoryKey(event)) {
        gate.endGesture();
        clearHistoryBottomCompensation();
        if (keyIntentTimeoutRef.current) {
          clearTimeout(keyIntentTimeoutRef.current);
          keyIntentTimeoutRef.current = null;
        }
        return;
      }
      if (!isBackwardHistoryKey(event)) return;

      gate.beginGesture();
      maybeLoadOlderMessages();

      if (keyIntentTimeoutRef.current) {
        clearTimeout(keyIntentTimeoutRef.current);
      }
      keyIntentTimeoutRef.current = setTimeout(() => {
        gate.endGesture();
        keyIntentTimeoutRef.current = null;
      }, 500);
    };

    const handleHistoryKeyUp = (event: KeyboardEvent) => {
      if (!isBackwardHistoryKey(event)) return;

      if (keyIntentTimeoutRef.current) {
        clearTimeout(keyIntentTimeoutRef.current);
        keyIntentTimeoutRef.current = null;
      }
      gate.endGesture();
    };

    // Chrome on macOS exposes the first-event cancelability boundary only when
    // the listener is non-passive. We never prevent the event, so native
    // scrolling is unchanged. Tauri and other platforms retain the passive
    // listener because WKWebView does not provide the same stable boundary.
    container.addEventListener("wheel", handleHistoryWheel, {
      passive: !usesMacOSWheelGestureStart
    });
    container.addEventListener("touchstart", handleHistoryTouchStart, { passive: true });
    container.addEventListener("touchmove", handleHistoryTouchMove, { passive: true });
    container.addEventListener("touchend", handleHistoryTouchEnd, { passive: true });
    container.addEventListener("touchcancel", handleHistoryTouchCancel, { passive: true });
    container.addEventListener("pointerdown", handleHistoryPointerDown);
    container.addEventListener("scroll", handleHistoryScroll, { passive: true });
    container.addEventListener("scrollend", handleHistoryScrollEnd);
    window.addEventListener("pointerup", handleHistoryPointerEnd);
    window.addEventListener("pointercancel", handleHistoryPointerEnd);
    window.addEventListener("keydown", handleHistoryKeyDown);
    window.addEventListener("keyup", handleHistoryKeyUp);

    return () => {
      container.removeEventListener("wheel", handleHistoryWheel);
      container.removeEventListener("touchstart", handleHistoryTouchStart);
      container.removeEventListener("touchmove", handleHistoryTouchMove);
      container.removeEventListener("touchend", handleHistoryTouchEnd);
      container.removeEventListener("touchcancel", handleHistoryTouchCancel);
      container.removeEventListener("pointerdown", handleHistoryPointerDown);
      container.removeEventListener("scroll", handleHistoryScroll);
      container.removeEventListener("scrollend", handleHistoryScrollEnd);
      window.removeEventListener("pointerup", handleHistoryPointerEnd);
      window.removeEventListener("pointercancel", handleHistoryPointerEnd);
      window.removeEventListener("keydown", handleHistoryKeyDown);
      window.removeEventListener("keyup", handleHistoryKeyUp);
    };
  }, [
    clearHistoryBottomCompensation,
    historyPaginationGate,
    isTauriEnv,
    isVisible,
    maybeLoadOlderMessages
  ]);

  // Observe a persistent list-top sentinel rather than the first rendered turn. A long
  // assistant turn can overlap the viewport even when its actual top is far above it.
  useEffect(() => {
    const container = chatContainerRef.current;
    const sentinel = historyTopSentinelRef.current;
    if (!container || !sentinel || !hasMoreOlderMessages) return;

    const observer = new IntersectionObserver(
      (entries) => {
        if (entries[0]?.isIntersecting) {
          maybeLoadOlderMessages();
        }
      },
      {
        root: container,
        rootMargin: `${CHAT_HISTORY_TOP_MARGIN_PX}px 0px 0px 0px`,
        threshold: 0
      }
    );

    observer.observe(sentinel);

    return () => {
      observer.disconnect();
    };
  }, [hasMoreOlderMessages, maybeLoadOlderMessages]);

  // Auto-clear error after 3 seconds
  useEffect(() => {
    if (error) {
      const ownerKey = activeRuntimeKey;
      const ownerError = error;
      const timeoutId = setTimeout(() => {
        if (runtimeStore.get(ownerKey)?.error === ownerError) {
          setErrorForKey(ownerKey, null);
        }
      }, 3000);

      return () => clearTimeout(timeoutId);
    }
  }, [activeRuntimeKey, error, runtimeStore, setErrorForKey]);

  // Toggle sidebar
  const toggleSidebar = useCallback(() => setIsSidebarOpen((prev) => !prev), [setIsSidebarOpen]);

  const handleNewChatFromHeader = useCallback(() => {
    flushSync(() => {
      setSelectedProjectId(null);
    });

    const usp = new URLSearchParams(window.location.search);
    usp.delete("conversation_id");
    usp.delete("project_id");
    const newUrl = usp.toString()
      ? `${window.location.pathname}?${usp.toString()}`
      : window.location.pathname;
    const draftRuntimeKey = resumeOrCreateChatDraftKey(runtimeStore, null);
    const chatEntry = createChatHistoryEntryForDraft(draftRuntimeKey);
    window.history.pushState(chatEntry.historyState, "", newUrl);
    window.dispatchEvent(
      new CustomEvent<NewChatNavigationDetail>("newchat", {
        detail: { projectId: null, draftRuntimeKey: chatEntry.draftRuntimeKey }
      })
    );
    if (isSidebarOpen) {
      toggleSidebar();
    }
  }, [isSidebarOpen, runtimeStore, setSelectedProjectId, toggleSidebar]);

  const handleNewChatFromUpgrade = useCallback(() => {
    const projectId = selectedProjectId ?? null;
    const draftRuntimeKey = resumeOrCreateChatDraftKey(runtimeStore, projectId);
    const chatEntry = createChatHistoryEntryForDraft(draftRuntimeKey);
    const params = new URLSearchParams(window.location.search);
    params.delete("conversation_id");
    const url = params.toString() ? `/?${params.toString()}` : "/";
    window.history.replaceState(chatEntry.historyState, "", url);
    window.dispatchEvent(
      new CustomEvent<NewChatNavigationDetail>("newchat", {
        detail: { projectId, draftRuntimeKey: chatEntry.draftRuntimeKey }
      })
    );
  }, [runtimeStore, selectedProjectId]);

  // Check user's billing access
  const hasProAccess =
    billingStatus &&
    (billingStatus.product_name?.toLowerCase().includes("pro") ||
      billingStatus.product_name?.toLowerCase().includes("max") ||
      billingStatus.product_name?.toLowerCase().includes("team"));

  const canUseImages = hasProAccess;
  const canUseDocuments = hasProAccess;
  const canUseVoice = hasProAccess && hasWhisperModel;

  const setComposerErrorForKey = useCallback(
    (
      key: ChatRuntimeKey,
      field: "attachmentError" | "audioError",
      message: string | null,
      duration = 5000
    ) => {
      if (!updateComposerForKey(key, (composer) => ({ ...composer, [field]: message }))) return;
      if (!message) return;

      setTimeout(() => {
        const snapshot = runtimeStore.get(key);
        if (snapshot?.composer[field] !== message) return;
        updateComposerForKey(key, (composer) => ({ ...composer, [field]: null }));
      }, duration);
    },
    [runtimeStore, updateComposerForKey]
  );

  const handleAddImages = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      const ownerKey = fileInputOwnerKeyRef.current ?? activeRuntimeKeyRef.current;
      fileInputOwnerKeyRef.current = null;
      const selectedFiles = Array.from(e.currentTarget.files ?? []);
      e.currentTarget.value = "";
      const ownerSnapshot = runtimeStore.get(ownerKey);
      if (selectedFiles.length === 0 || !ownerSnapshot || ownerSnapshot.composer.queue.edit) return;

      const supportedTypes = ["image/jpeg", "image/jpg", "image/png", "image/webp"];
      const maxSizeInBytes = 20 * 1024 * 1024;
      let validationError: string | null = null;

      const validFiles = selectedFiles.filter((file) => {
        if (!supportedTypes.includes(file.type.toLowerCase())) {
          validationError = "Only JPEG, PNG, and WebP images are supported";
          return false;
        }
        if (file.size > maxSizeInBytes) {
          validationError = "Image too large (max 20MB)";
          return false;
        }
        return true;
      });
      if (validFiles.length === 0) {
        if (validationError) {
          setComposerErrorForKey(ownerKey, "attachmentError", validationError);
        }
        return;
      }
      const accountUsage = chatAccountQueueUsage(runtimeStore);
      const selection = selectChatImageFilesForRetention({
        composer: ownerSnapshot.composer,
        candidates: validFiles,
        accountUsage
      });
      if (selection.accountLimitExceeded) {
        setComposerErrorForKey(ownerKey, "attachmentError", CHAT_ACCOUNT_IMAGE_LIMIT_MESSAGE);
      } else if (selection.messageLimitExceeded) {
        setComposerErrorForKey(ownerKey, "attachmentError", CHAT_MESSAGE_IMAGE_LIMIT_MESSAGE);
      } else if (validationError) {
        setComposerErrorForKey(ownerKey, "attachmentError", validationError);
      }
      if (selection.files.length === 0) return;
      const additionalBytes = selection.files.reduce((total, file) => total + file.size, 0);
      if (
        accountUsage.attachmentBytes + additionalBytes >
        MAX_CHAT_ACCOUNT_RETAINED_ATTACHMENT_BYTES
      ) {
        setComposerErrorForKey(ownerKey, "attachmentError", CHAT_ACCOUNT_ATTACHMENT_LIMIT_MESSAGE);
        return;
      }

      const newUrls = selection.files.map((file) => [file, URL.createObjectURL(file)] as const);
      const attached = updateAttachmentComposerForKey(ownerKey, (composer) => ({
        ...composer,
        imageUrls: new Map([...composer.imageUrls, ...newUrls]),
        draftImages: [...composer.draftImages, ...selection.files]
      }));
      if (!attached) for (const [, url] of newUrls) URL.revokeObjectURL(url);
    },
    [runtimeStore, setComposerErrorForKey, updateAttachmentComposerForKey]
  );

  const attachPastedImages = useCallback(
    (imageFiles: File[], ownerKey: ChatRuntimeKey, expectedGeneration: number) => {
      const ownerSnapshot = runtimeStore.get(ownerKey);
      if (!ownerSnapshot || ownerSnapshot.composer.queue.edit) return;

      if (!canUseImages) {
        if (isRuntimeSelected(ownerKey)) {
          setUpgradeFeature("image");
          setUpgradeDialogOpen(true);
        }
        return;
      }

      const supportedTypes = ["image/jpeg", "image/jpg", "image/png", "image/webp"];
      const maxSizeInBytes = 20 * 1024 * 1024;
      let validationError: string | null = null;

      const validFiles = imageFiles.filter((file) => {
        if (!supportedTypes.includes(file.type.toLowerCase())) {
          validationError = "Only JPEG, PNG, and WebP images are supported";
          return false;
        }
        if (file.size > maxSizeInBytes) {
          validationError = "Image too large (max 20MB)";
          return false;
        }
        return true;
      });
      if (validFiles.length === 0) {
        if (validationError) {
          setComposerErrorForKey(ownerKey, "attachmentError", validationError);
        }
        return;
      }
      const accountUsage = chatAccountQueueUsage(runtimeStore);
      const selection = selectChatImageFilesForRetention({
        composer: ownerSnapshot.composer,
        candidates: validFiles,
        accountUsage
      });
      if (selection.accountLimitExceeded) {
        setComposerErrorForKey(ownerKey, "attachmentError", CHAT_ACCOUNT_IMAGE_LIMIT_MESSAGE);
      } else if (selection.messageLimitExceeded) {
        setComposerErrorForKey(ownerKey, "attachmentError", CHAT_MESSAGE_IMAGE_LIMIT_MESSAGE);
      } else if (validationError) {
        setComposerErrorForKey(ownerKey, "attachmentError", validationError);
      }
      if (selection.files.length === 0) return;
      const additionalBytes = selection.files.reduce((total, file) => total + file.size, 0);
      if (
        accountUsage.attachmentBytes + additionalBytes >
        MAX_CHAT_ACCOUNT_RETAINED_ATTACHMENT_BYTES
      ) {
        setComposerErrorForKey(ownerKey, "attachmentError", CHAT_ACCOUNT_ATTACHMENT_LIMIT_MESSAGE);
        return;
      }

      const newUrls = selection.files.map((file) => [file, URL.createObjectURL(file)] as const);
      let generationMatched = false;
      const attached = updateAttachmentComposerForKey(ownerKey, (composer) => {
        if (composer.imagePasteGeneration !== expectedGeneration) return composer;
        generationMatched = true;
        return {
          ...composer,
          imageUrls: new Map([...composer.imageUrls, ...newUrls]),
          draftImages: [...composer.draftImages, ...selection.files]
        };
      });
      if (!attached || !generationMatched) {
        for (const [, url] of newUrls) URL.revokeObjectURL(url);
      }
    },
    [
      canUseImages,
      isRuntimeSelected,
      runtimeStore,
      setComposerErrorForKey,
      updateAttachmentComposerForKey
    ]
  );

  const handlePaste = useCallback(
    (e: React.ClipboardEvent) => {
      const ownerKey = activeRuntimeKeyRef.current;
      const startSnapshot = runtimeStore.get(ownerKey);
      if (!startSnapshot || startSnapshot.composer.queue.edit) return;
      const pasteGeneration = startSnapshot.composer.imagePasteGeneration + 1;
      updateComposerForKey(ownerKey, (composer) => ({
        ...composer,
        imagePasteGeneration: pasteGeneration
      }));
      const items = e.clipboardData?.items;
      if (!items) return;

      const imageFiles = getImageFilesFromClipboardItems(items);
      if (imageFiles.length > 0) {
        e.preventDefault();
        attachPastedImages(imageFiles, ownerKey, pasteGeneration);
        return;
      }

      if (!isLinuxTauriEnv) return;

      const fallback = maybeReadLinuxTauriClipboardImages({
        eventItemTypes: Array.from(items, (item) => item.type),
        isTauri: isTauriEnv,
        isLinux: isLinuxEnv,
        readClipboard: () => {
          if (typeof navigator === "undefined") return undefined;

          const clipboard = navigator.clipboard;
          if (typeof clipboard?.read !== "function") return undefined;
          return clipboard.read();
        }
      });

      void fallback?.then((fallbackFiles) => {
        const snapshot = runtimeStore.get(ownerKey);
        if (
          fallbackFiles.length === 0 ||
          snapshot?.composer.imagePasteGeneration !== pasteGeneration
        )
          return;
        attachPastedImages(fallbackFiles, ownerKey, pasteGeneration);
      });
    },
    [
      attachPastedImages,
      isLinuxEnv,
      isLinuxTauriEnv,
      isTauriEnv,
      runtimeStore,
      updateComposerForKey
    ]
  );

  const removeImage = useCallback(
    (idx: number) => {
      const ownerKey = activeRuntimeKeyRef.current;
      const snapshot = runtimeStore.get(ownerKey);
      if (!snapshot || snapshot.composer.queue.edit) return;
      const fileToRemove = snapshot?.composer.draftImages[idx];
      if (!fileToRemove) return;
      const url = snapshot.composer.imageUrls.get(fileToRemove);
      const removed = updateAttachmentComposerForKey(ownerKey, (composer) => {
        const nextUrls = new Map(composer.imageUrls);
        nextUrls.delete(fileToRemove);
        return {
          ...composer,
          imageUrls: nextUrls,
          draftImages: composer.draftImages.filter((file) => file !== fileToRemove)
        };
      });
      if (removed && url) URL.revokeObjectURL(url);
    },
    [runtimeStore, updateAttachmentComposerForKey]
  );

  const handleDocumentUpload = useCallback(
    async (e: React.ChangeEvent<HTMLInputElement>) => {
      const ownerKey = documentInputOwnerKeyRef.current ?? activeRuntimeKeyRef.current;
      documentInputOwnerKeyRef.current = null;
      const inputElement = e.currentTarget;
      const file = inputElement.files?.[0];
      if (!file) return;

      const startSnapshot = runtimeStore.get(ownerKey);
      if (!startSnapshot || startSnapshot.composer.queue.edit) {
        inputElement.value = "";
        return;
      }

      const maxSizeInBytes = 10 * 1024 * 1024;
      if (file.size > maxSizeInBytes) {
        setComposerErrorForKey(ownerKey, "attachmentError", "Document too large (max 10MB)");
        inputElement.value = "";
        return;
      }

      const uploadGeneration = startSnapshot.composer.documentUploadGeneration + 1;
      const started = updateAttachmentComposerForKey(ownerKey, (composer) => ({
        ...composer,
        isProcessingDocument: true,
        attachmentError: null,
        documentUploadGeneration: uploadGeneration
      }));
      if (!started) {
        inputElement.value = "";
        return;
      }

      const updateDocumentIfCurrent = (
        updater: (composer: ChatComposerState) => ChatComposerState
      ) => {
        let generationMatched = false;
        const updated = updateAttachmentComposerForKey(ownerKey, (composer) => {
          if (composer.documentUploadGeneration !== uploadGeneration) return composer;
          generationMatched = true;
          return updater(composer);
        });
        return updated && generationMatched;
      };
      const retainDocumentIfWithinAccountBudget = (documentText: string, documentName: string) =>
        updateDocumentIfCurrent((composer) => {
          const accountUsage = chatAccountQueueUsage(runtimeStore);
          const nextAttachmentBytes =
            accountUsage.attachmentBytes -
            chatQueuedTextByteLength(composer.documentText) +
            chatQueuedTextByteLength(documentText);
          if (nextAttachmentBytes > MAX_CHAT_ACCOUNT_RETAINED_ATTACHMENT_BYTES) {
            return {
              ...composer,
              attachmentError: CHAT_ACCOUNT_ATTACHMENT_LIMIT_MESSAGE
            };
          }
          return { ...composer, documentText, documentName };
        });

      try {
        const documentType = getSupportedDocumentType(file.name);

        if (documentType === "txt" || documentType === "md") {
          const text = await file.text();
          const documentData = {
            document: {
              filename: file.name,
              text_content: text
            }
          };
          retainDocumentIfWithinAccountBudget(JSON.stringify(documentData), file.name);
        } else if (documentType && isNativeDocumentType(documentType) && isTauriEnv) {
          const result = await extractDocumentContent(file, documentType);
          if (runtimeStore.get(ownerKey)?.composer.documentUploadGeneration !== uploadGeneration)
            return;

          const cleanedText =
            documentType === "pdf"
              ? prepareExtractedPdfText(result.document?.text_content)
              : prepareExtractedDocumentText(result.document?.text_content);
          if (cleanedText === null) {
            setComposerErrorForKey(
              ownerKey,
              "attachmentError",
              documentType === "pdf"
                ? "No readable text was found in this PDF"
                : "No readable text was found in this Word document"
            );
            return;
          }

          const cleanedParsed = {
            document: {
              filename: result.document.filename,
              text_content: cleanedText
            }
          };

          retainDocumentIfWithinAccountBudget(JSON.stringify(cleanedParsed), file.name);
        } else if (documentType && isNativeDocumentType(documentType)) {
          setComposerErrorForKey(
            ownerKey,
            "attachmentError",
            "PDF and Word files can only be processed in the Maple app"
          );
        } else {
          setComposerErrorForKey(
            ownerKey,
            "attachmentError",
            "Only PDF, DOC, DOCX, TXT, and Markdown files are supported"
          );
        }
      } catch (error) {
        console.error("Document processing error:", error);
        if (runtimeStore.get(ownerKey)?.composer.documentUploadGeneration === uploadGeneration) {
          setComposerErrorForKey(
            ownerKey,
            "attachmentError",
            getDocumentProcessingErrorMessage(error)
          );
        }
      } finally {
        // An edit may begin while native document extraction is in flight.
        // Attachment mutation stays blocked during the edit, but the owned
        // processing flag must always settle so the composer cannot deadlock.
        updateComposerForKey(ownerKey, (composer) =>
          composer.documentUploadGeneration === uploadGeneration
            ? { ...composer, isProcessingDocument: false }
            : composer
        );
        inputElement.value = "";
      }
    },
    [
      isTauriEnv,
      runtimeStore,
      setComposerErrorForKey,
      updateAttachmentComposerForKey,
      updateComposerForKey
    ]
  );

  const removeDocument = useCallback(() => {
    const ownerKey = activeRuntimeKeyRef.current;
    updateAttachmentComposerForKey(ownerKey, (composer) => ({
      ...composer,
      isProcessingDocument: false,
      documentText: "",
      documentName: "",
      documentUploadGeneration: composer.documentUploadGeneration + 1
    }));
  }, [updateAttachmentComposerForKey]);

  // Audio recording functions
  const startRecording = async () => {
    if (isRecording || isTranscribing || recordingOwnerKeyRef.current) return;

    if (!canUseVoice) {
      setUpgradeFeature("voice");
      setUpgradeDialogOpen(true);
      return;
    }

    const ownerKey = runtimeStore.resolveKey(activeRuntimeKeyRef.current);
    const ownerSessionToken = recordingSessionTokenRef.current + 1;
    recordingSessionTokenRef.current = ownerSessionToken;
    recordingOwnerKeyRef.current = ownerKey;
    setRecordingOwnerKey(ownerKey);

    try {
      if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
        setComposerErrorForKey(
          ownerKey,
          "audioError",
          "Microphone access is blocked. Please check your browser permissions or disable Lockdown Mode for this site (Settings > Safari > Advanced > Lockdown Mode).",
          8000
        );
        if (
          isRecordingOwnershipCurrent(
            ownerKey,
            ownerSessionToken,
            recordingOwnerKeyRef.current,
            recordingSessionTokenRef.current,
            Boolean(runtimeStore.get(ownerKey))
          )
        ) {
          recordingOwnerKeyRef.current = null;
          recordingSessionTokenRef.current += 1;
          setRecordingOwnerKey(null);
        }
        return;
      }

      const stream = await navigator.mediaDevices.getUserMedia({
        audio: {
          echoCancellation: false,
          noiseSuppression: true,
          autoGainControl: false,
          sampleRate: 16000
        }
      });
      if (
        !isRecordingOwnershipCurrent(
          ownerKey,
          ownerSessionToken,
          recordingOwnerKeyRef.current,
          recordingSessionTokenRef.current,
          Boolean(runtimeStore.get(ownerKey))
        )
      ) {
        stream.getTracks().forEach((track) => track.stop());
        if (
          recordingOwnerKeyRef.current === ownerKey &&
          recordingSessionTokenRef.current === ownerSessionToken
        ) {
          recordingOwnerKeyRef.current = null;
          recordingSessionTokenRef.current += 1;
          setRecordingOwnerKey(null);
        }
        return;
      }

      streamRef.current = stream;

      const recorder = new RecordRTC(stream, {
        type: "audio",
        mimeType: "audio/wav",
        recorderType: RecordRTC.StereoAudioRecorder,
        numberOfAudioChannels: 1,
        desiredSampRate: 16000
      });

      recorderRef.current = recorder;
      recorder.startRecording();
      setIsRecording(true);
      setComposerErrorForKey(ownerKey, "audioError", null);
    } catch (error) {
      console.error("Failed to start recording:", error);
      if (
        !isRecordingOwnershipCurrent(
          ownerKey,
          ownerSessionToken,
          recordingOwnerKeyRef.current,
          recordingSessionTokenRef.current,
          Boolean(runtimeStore.get(ownerKey))
        )
      ) {
        return;
      }
      const err = error as Error & { name?: string };
      let errorMessage: string;

      if (err.name === "NotAllowedError" || err.name === "PermissionDeniedError") {
        errorMessage =
          "Microphone access denied. Please enable microphone permissions in Settings > Maple.";
      } else if (err.name === "NotFoundError" || err.name === "DevicesNotFoundError") {
        errorMessage = "No microphone found. Please check your device.";
      } else if (err.name === "NotReadableError" || err.name === "TrackStartError") {
        errorMessage = "Microphone is already in use by another app.";
      } else {
        errorMessage = `Failed to access microphone: ${err.name || "Unknown error"} - ${err.message || "Please try again"}`;
      }
      const failedRecorder = recorderRef.current;
      const failedStream = streamRef.current;
      const cleanupResult = cleanupRecordingForTeardown({
        recorder: failedRecorder
          ? { stopRecording: (callback) => failedRecorder.stopRecording(callback) }
          : null,
        stream: failedStream,
        clearOwnership: () => {
          if (
            recordingOwnerKeyRef.current === ownerKey &&
            recordingSessionTokenRef.current === ownerSessionToken
          ) {
            recordingOwnerKeyRef.current = null;
            recordingSessionTokenRef.current += 1;
            setRecordingOwnerKey(null);
            setIsRecording(false);
            setIsTranscribing(false);
            setIsProcessingSend(false);
          }
        },
        clearRecorder: () => {
          if (recorderRef.current === failedRecorder) recorderRef.current = null;
        },
        clearStream: () => {
          if (streamRef.current === failedStream) streamRef.current = null;
        }
      });
      if (cleanupResult.errors.length > 0) {
        console.error("Failed to fully clean up microphone startup:", cleanupResult.errors);
      }
      setComposerErrorForKey(ownerKey, "audioError", errorMessage);
    }
  };

  const stopRecording = (shouldSend: boolean = false) => {
    const recorder = recorderRef.current;
    const ownerKey = recordingOwnerKeyRef.current;
    const ownerSessionToken = recordingSessionTokenRef.current;
    if (recorder && ownerKey && isRecording) {
      const ownedStream = streamRef.current;
      if (!shouldSend) {
        setIsRecording(false);
      } else {
        setIsProcessingSend(true);
      }

      recorder.stopRecording(async () => {
        let released = false;

        const releaseRecording = () => {
          if (released) return;
          released = true;
          ownedStream?.getTracks().forEach((track) => track.stop());
          if (streamRef.current === ownedStream) {
            streamRef.current = null;
          }
          if (recorderRef.current === recorder) recorderRef.current = null;
          if (
            recordingOwnerKeyRef.current === ownerKey &&
            recordingSessionTokenRef.current === ownerSessionToken
          ) {
            recordingOwnerKeyRef.current = null;
            recordingSessionTokenRef.current += 1;
            setRecordingOwnerKey((current) => (current === ownerKey ? null : current));
            setIsProcessingSend(false);
            setIsTranscribing(false);
            setIsRecording(false);
          }
        };

        if (
          !isRecordingOwnershipCurrent(
            ownerKey,
            ownerSessionToken,
            recordingOwnerKeyRef.current,
            recordingSessionTokenRef.current,
            Boolean(runtimeStore.get(ownerKey))
          )
        ) {
          releaseRecording();
          return;
        }

        const blob = recorder.getBlob();
        if (!blob || blob.size === 0) {
          console.error("No audio recorded or empty recording");
          if (shouldSend) {
            setComposerErrorForKey(
              ownerKey,
              "audioError",
              "No audio was recorded. Please try again."
            );
          }
          releaseRecording();
          return;
        }

        const audioFile = new File([blob], "recording.wav", {
          type: "audio/wav"
        });

        if (shouldSend) {
          setIsTranscribing(true);
          try {
            const result = await os.transcribeAudio(audioFile, "whisper-large-v3");
            if (
              !isRecordingOwnershipCurrent(
                ownerKey,
                ownerSessionToken,
                recordingOwnerKeyRef.current,
                recordingSessionTokenRef.current,
                Boolean(runtimeStore.get(ownerKey))
              )
            ) {
              return;
            }
            const transcribedText = result.text.trim();

            if (transcribedText) {
              const ownerInput = runtimeStore.get(ownerKey)?.composer.input ?? "";
              const newValue = ownerInput ? `${ownerInput} ${transcribedText}` : transcribedText;
              const send = handleSendMessage(undefined, newValue, ownerKey);
              releaseRecording();
              await send;
            } else {
              setComposerErrorForKey(
                ownerKey,
                "audioError",
                "No speech detected. Please try again."
              );
            }
          } catch (error) {
            console.error("Transcription failed:", error);
            if (
              isRecordingOwnershipCurrent(
                ownerKey,
                ownerSessionToken,
                recordingOwnerKeyRef.current,
                recordingSessionTokenRef.current,
                Boolean(runtimeStore.get(ownerKey))
              )
            ) {
              setComposerErrorForKey(
                ownerKey,
                "audioError",
                "Failed to transcribe audio. Please try again."
              );
            }
          } finally {
            releaseRecording();
          }
        } else {
          releaseRecording();
        }
      });
    }
  };

  const discardQueueEdit = useCallback(() => {
    const runtimeKey = runtimeStore.resolveKey(activeRuntimeKeyRef.current);
    if (!runtimeStore.get(runtimeKey)) return;
    runtimeStore.update(runtimeKey, (snapshot) => {
      const discarded = discardChatQueuedMessageEdit(snapshot.composer.queue);
      if (discarded.status !== "ended") return snapshot;
      return {
        ...snapshot,
        composer: {
          ...snapshot.composer,
          input: discarded.restoreInput,
          queue: discarded.queue
        }
      };
    });
  }, [runtimeStore]);

  const cancelQueuedMessage = useCallback(
    (queueId: string) => {
      const runtimeKey = runtimeStore.resolveKey(activeRuntimeKeyRef.current);
      let removedItem: ChatQueuedMessage | null = null;
      let retainedObjectUrls = new Set<string>();
      if (!runtimeStore.get(runtimeKey)) return;
      runtimeStore.update(runtimeKey, (snapshot) => {
        const cancelled = cancelChatQueuedMessage(snapshot.composer.queue, queueId);
        if (cancelled.status !== "cancelled") return snapshot;
        removedItem = cancelled.item;
        const composer = {
          ...snapshot.composer,
          input: cancelled.restoreInput ?? snapshot.composer.input,
          queue: cancelled.queue
        };
        retainedObjectUrls = new Set([
          ...chatComposerObjectUrls(composer),
          ...chatTranscriptObjectUrls(snapshot.messages as Message[])
        ]);
        return {
          ...snapshot,
          composer
        };
      });
      if (removedItem) {
        revokeQueuedChatMessageObjectUrls(removedItem, retainedObjectUrls);
      }
    },
    [runtimeStore]
  );

  const editQueuedMessage = useCallback(
    (queueId: string) => {
      const runtimeKey = runtimeStore.resolveKey(activeRuntimeKeyRef.current);
      let shouldFocus = false;
      if (!runtimeStore.get(runtimeKey)) return;
      runtimeStore.update(runtimeKey, (snapshot) => {
        const started = beginChatQueuedMessageEdit(
          snapshot.composer.queue,
          runtimeKey,
          queueId,
          snapshot.composer.input
        );
        if (started.status === "already_editing") {
          const discarded = discardChatQueuedMessageEdit(snapshot.composer.queue);
          if (discarded.status !== "ended") return snapshot;
          return {
            ...snapshot,
            composer: {
              ...snapshot.composer,
              input: discarded.restoreInput,
              queue: discarded.queue
            }
          };
        }
        if (started.status !== "started") return snapshot;
        shouldFocus = true;
        return {
          ...snapshot,
          composer: {
            ...snapshot.composer,
            input: started.input,
            queue: started.queue
          }
        };
      });
      if (shouldFocus) requestAnimationFrame(() => textareaRef.current?.focus());
    },
    [runtimeStore]
  );

  // Helper function to process one streaming response.
  const processStreamingResponse = useCallback(
    async (
      stream: AsyncIterable<unknown>,
      runtimeKey: ChatRuntimeKey,
      runToken: number,
      optimisticMessageId: string,
      onResponseCreated: (responseId: string) => Promise<boolean>
    ): Promise<ChatStreamTerminalState> => {
      const messageTextBuffers = new Map<string, Map<number, string>>();
      const reasoningTextBuffers = new Map<string, Map<number, string>>();
      const ownedItemIds = new Set<string>();
      const streamStartedAt = performance.now();
      let streamEventCount = 0;
      let terminalState: ChatStreamTerminalState = null;

      const updateRunMessages = (updater: (messages: Message[]) => Message[]) =>
        runtimeStore.updateForRun(runtimeKey, runToken, (snapshot) => ({
          ...snapshot,
          messages: updater(snapshot.messages as Message[])
        }));

      const appendBufferedText = (
        buffers: Map<string, Map<number, string>>,
        itemId: string,
        contentIndex: number,
        delta: string
      ) => {
        const contentBuffers = buffers.get(itemId) ?? new Map<number, string>();
        const nextText = `${contentBuffers.get(contentIndex) ?? ""}${delta}`;
        contentBuffers.set(contentIndex, nextText);
        buffers.set(itemId, contentBuffers);
        return nextText;
      };

      const setBufferedText = (
        buffers: Map<string, Map<number, string>>,
        itemId: string,
        contentIndex: number,
        text: string
      ) => {
        const contentBuffers = buffers.get(itemId) ?? new Map<number, string>();
        contentBuffers.set(contentIndex, text);
        buffers.set(itemId, contentBuffers);
        return text;
      };

      const deltaCoalescer = createChatStreamDeltaCoalescer({
        isCurrent: () => runtimeStore.isRunCurrent(runtimeKey, runToken),
        onFlush: (deltas) => {
          runtimeStore.updateForRun(runtimeKey, runToken, (snapshot) => {
            let nextMessages = snapshot.messages as Message[];

            for (const delta of deltas) {
              if (delta.kind === "reasoning") {
                const nextText = appendBufferedText(
                  reasoningTextBuffers,
                  delta.itemId,
                  delta.contentIndex,
                  delta.delta
                );
                nextMessages = updateMessageById(nextMessages, delta.itemId, (message) =>
                  message.type === "reasoning"
                    ? upsertReasoningTextContent(
                        message as ReasoningItem,
                        delta.contentIndex,
                        nextText,
                        "streaming"
                      )
                    : message
                );
              } else {
                const nextText = appendBufferedText(
                  messageTextBuffers,
                  delta.itemId,
                  delta.contentIndex,
                  delta.delta
                );
                nextMessages = updateMessageById(nextMessages, delta.itemId, (message) =>
                  message.type === "message"
                    ? (upsertAssistantTextContent(
                        message as ExtendedMessage,
                        delta.contentIndex,
                        nextText,
                        "streaming"
                      ) as unknown as Message)
                    : message
                );
              }
            }

            return { ...snapshot, messages: nextMessages };
          });
        }
      });
      registerChatStreamDeltaCoalescer(runtimeStore, runToken, deltaCoalescer);

      try {
        for await (const event of stream) {
          const eventType = (event as { type: string }).type;
          streamEventCount += 1;
          logStreamEvent(runtimeKey, runToken, streamStartedAt, streamEventCount, eventType, event);

          if (
            eventType === "response.reasoning_text.delta" &&
            (event as { delta?: string }).delta
          ) {
            const reasoningEvent = event as ResponseReasoningTextDeltaEvent;
            ownedItemIds.add(reasoningEvent.item_id);
            deltaCoalescer.enqueue({
              kind: "reasoning",
              itemId: reasoningEvent.item_id,
              contentIndex: reasoningEvent.content_index,
              delta: reasoningEvent.delta
            });
            continue;
          }

          if (eventType === "response.output_text.delta" && (event as { delta?: string }).delta) {
            const textEvent = event as ResponseTextDeltaEvent;
            ownedItemIds.add(textEvent.item_id);
            deltaCoalescer.enqueue({
              kind: "message",
              itemId: textEvent.item_id,
              contentIndex: textEvent.content_index,
              delta: textEvent.delta
            });
            continue;
          }

          // Preserve event ordering: lifecycle, tool, terminal, and exact-text
          // events must observe every text delta that arrived before them.
          deltaCoalescer.flush();

          if (eventType === "response.created") {
            unregisterChatOptimisticMessage(runtimeStore, runToken, optimisticMessageId);
            const eventWithResponse = event as { response?: { id?: string } };
            const responseId = eventWithResponse.response?.id;
            if (responseId) {
              if (runtimeStore.setCurrentResponseId(runtimeKey, runToken, responseId)) {
                clearUnresolvedChatResponseMessage(runtimeStore, runToken, optimisticMessageId);
              }
              if (await onResponseCreated(responseId)) {
                terminalState = "cancelled";
                break;
              }
            }
          } else if (eventType === "response.output_item.added") {
            const addedEvent = event as ResponseOutputItemAddedEvent;
            const item = normalizeConversationItem(addedEvent.item);

            if (item) {
              ownedItemIds.add(item.id);
              updateRunMessages((messages) => mergeStreamingConversationItem(messages, item));
            }
          } else if (eventType === "response.web_search_call.in_progress") {
            const webSearchEvent = event as { item_id?: string };
            if (webSearchEvent.item_id) {
              ownedItemIds.add(webSearchEvent.item_id);
              updateRunMessages((messages) =>
                updateMessageById(messages, webSearchEvent.item_id!, (message) =>
                  message.type === "web_search_call"
                    ? ({ ...message, status: "in_progress" } as unknown as Message)
                    : message
                )
              );
            }
          } else if (eventType === "response.web_search_call.searching") {
            const webSearchEvent = event as { item_id?: string };
            if (webSearchEvent.item_id) {
              ownedItemIds.add(webSearchEvent.item_id);
              updateRunMessages((messages) =>
                updateMessageById(messages, webSearchEvent.item_id!, (message) =>
                  message.type === "web_search_call"
                    ? ({ ...message, status: "searching" } as unknown as Message)
                    : message
                )
              );
            }
          } else if (eventType === "response.web_search_call.completed") {
            const webSearchEvent = event as { item_id?: string };
            if (webSearchEvent.item_id) {
              ownedItemIds.add(webSearchEvent.item_id);
              updateRunMessages((messages) =>
                updateMessageById(messages, webSearchEvent.item_id!, (message) =>
                  message.type === "web_search_call"
                    ? ({ ...message, status: "completed" } as unknown as Message)
                    : message
                )
              );
            }
          } else if (eventType === "tool_call.created") {
            const toolCallEvent = event as {
              tool_call_id?: string;
              name?: string;
              arguments?: { query?: string };
            };
            if (toolCallEvent.tool_call_id) {
              ownedItemIds.add(toolCallEvent.tool_call_id);
              const toolCallItem: ToolCallItem = {
                id: toolCallEvent.tool_call_id,
                call_id: toolCallEvent.tool_call_id,
                type: "tool_call",
                name: toolCallEvent.name || "function",
                arguments: JSON.stringify(toolCallEvent.arguments || {}),
                status: "in_progress"
              };

              updateRunMessages((messages) => {
                const existingToolCall = messages.find(
                  (message) => message.id === toolCallEvent.tool_call_id
                );

                if (existingToolCall && isToolCallItem(existingToolCall)) {
                  return mergeMessagesById(messages, [
                    {
                      ...existingToolCall,
                      ...toolCallItem,
                      status: existingToolCall.status || toolCallItem.status
                    }
                  ]);
                }

                return mergeMessagesById(messages, [toolCallItem]);
              });
            }
          } else if (eventType === "tool_output.created") {
            const toolOutputEvent = event as {
              tool_output_id?: string;
              tool_call_id?: string;
              output?: string;
            };
            if (toolOutputEvent.tool_output_id && toolOutputEvent.tool_call_id) {
              ownedItemIds.add(toolOutputEvent.tool_output_id);
              ownedItemIds.add(toolOutputEvent.tool_call_id);
              const toolOutputItem: ToolOutputItem = {
                id: toolOutputEvent.tool_output_id,
                call_id: toolOutputEvent.tool_call_id,
                type: "tool_output",
                output: toolOutputEvent.output || "",
                status: "completed"
              };

              updateRunMessages((messages) => {
                const existingToolOutput = messages.find(
                  (message) => message.id === toolOutputEvent.tool_output_id
                );
                const withOutput = mergeMessagesById(messages, [
                  existingToolOutput && isToolOutputItem(existingToolOutput)
                    ? {
                        ...existingToolOutput,
                        ...toolOutputItem
                      }
                    : toolOutputItem
                ]);

                return updateMessageById(withOutput, toolOutputEvent.tool_call_id!, (message) =>
                  isToolCallItem(message)
                    ? ({ ...message, status: "completed" } as unknown as Message)
                    : message
                );
              });
            }
          } else if (eventType === "response.reasoning_text.done") {
            const reasoningEvent = event as ResponseReasoningTextDoneEvent;
            const nextText = setBufferedText(
              reasoningTextBuffers,
              reasoningEvent.item_id,
              reasoningEvent.content_index,
              reasoningEvent.text
            );

            ownedItemIds.add(reasoningEvent.item_id);
            updateRunMessages((messages) =>
              updateMessageById(messages, reasoningEvent.item_id, (message) =>
                message.type === "reasoning"
                  ? upsertReasoningTextContent(
                      message as ReasoningItem,
                      reasoningEvent.content_index,
                      nextText,
                      "completed"
                    )
                  : message
              )
            );
          } else if (eventType === "response.output_text.done") {
            const textEvent = event as ResponseTextDoneEvent;
            const nextText = setBufferedText(
              messageTextBuffers,
              textEvent.item_id,
              textEvent.content_index,
              textEvent.text
            );

            ownedItemIds.add(textEvent.item_id);
            updateRunMessages((messages) =>
              updateMessageById(messages, textEvent.item_id, (message) =>
                message.type === "message"
                  ? (upsertAssistantTextContent(
                      message as ExtendedMessage,
                      textEvent.content_index,
                      nextText,
                      "completed"
                    ) as unknown as Message)
                  : message
              )
            );
          } else if (eventType === "response.output_item.done") {
            const doneEvent = event as ResponseOutputItemDoneEvent;
            const item = normalizeConversationItem(doneEvent.item);

            if (item) {
              ownedItemIds.add(item.id);
              runtimeStore.updateForRun(runtimeKey, runToken, (snapshot) => ({
                ...snapshot,
                messages: mergeStreamingConversationItem(snapshot.messages as Message[], item),
                lastSeenItemId: item.id
              }));
            }
          } else if (eventType === "response.completed") {
            terminalState = "completed";
            // This is the authoritative terminal frame. Do not let a later
            // transport-close error turn a durable completion into a retry.
            break;
          } else if (isTerminalChatStreamErrorEvent(eventType)) {
            terminalState = "error";
            console.error("Streaming error:", event);
            runtimeStore.updateForRun(runtimeKey, runToken, (snapshot) => ({
              ...snapshot,
              messages: updateActiveItemStatuses(
                snapshot.messages as Message[],
                "error",
                ownedItemIds
              ),
              error: "Failed to generate response. Please try again."
            }));
            break;
          } else if (eventType === "response.cancelled") {
            terminalState = "cancelled";
            updateRunMessages((messages) =>
              updateActiveItemStatuses(messages, "incomplete", ownedItemIds)
            );
            break;
          }
        }

        if (
          classifyChatStreamEof(terminalState, runtimeStore.isRunCurrent(runtimeKey, runToken)) ===
          "truncated"
        ) {
          throw new Error("Response stream ended before completion");
        }
      } catch (error) {
        // Commit the final partial frame before changing its status. UI cancel
        // and account teardown clear run ownership before aborting, so their
        // resulting iterator errors remain stale-fenced here.
        deltaCoalescer.finish();
        updateRunMessages((messages) => updateActiveItemStatuses(messages, "error", ownedItemIds));
        throw error;
      } finally {
        // Natural EOF and thrown stream errors both commit the final partial
        // batch while the run is still owned. A cancelled/replaced token fails
        // the coalescer's current-run fence and is discarded instead.
        deltaCoalescer.finish();
        unregisterChatStreamDeltaCoalescer(runtimeStore, runToken, deltaCoalescer);
      }

      return terminalState;
    },
    [logStreamEvent, runtimeStore]
  );

  // Every submit synchronously detaches its owning composer. During an active
  // run that creates a staged chip; otherwise one outer run drains the FIFO as
  // separate Responses turns without an idle run-token or AbortController gap.
  const handleSendMessage = useCallback(
    async (e?: React.FormEvent, overrideInput?: string, ownerRuntimeKey?: ChatRuntimeKey) => {
      e?.preventDefault();
      let runtimeKey = runtimeStore.resolveKey(ownerRuntimeKey ?? activeRuntimeKeyRef.current);
      const startSnapshot = runtimeStore.get(runtimeKey);
      if (!startSnapshot) return;
      const retainOverrideInput = () => {
        if (overrideInput === undefined) return;
        runtimeStore.update(runtimeKey, (snapshot) => ({
          ...snapshot,
          composer: chatComposerWithInputOverride(snapshot.composer, overrideInput)
        }));
      };
      if (!openai || isChatRuntimeDeletionPending(runtimeStore, runtimeKey)) {
        retainOverrideInput();
        return;
      }
      const isRuntimeStopping = (runToken?: number) =>
        Array.from(stoppingRuntimeRegistry.getEntries()).some(
          ([key, runTokens]) =>
            (runToken === undefined || runTokens.has(runToken)) &&
            runtimeStore.resolveKey(key) === runtimeKey
        );
      if (isRuntimeStopping()) {
        retainOverrideInput();
        return;
      }

      const metadata: ChatQueuedMessageMetadata = {
        queueId: uuidv4(),
        messageId: uuidv4(),
        model: model || DEFAULT_MODEL_ID,
        webSearchEnabled: isWebSearchEnabled,
        createdMs: Date.now()
      };
      const composerAtSubmit = chatComposerWithInputOverride(startSnapshot.composer, overrideInput);
      const preflight = planChatComposerSubmission({
        composer: composerAtSubmit,
        hasActiveRun: startSnapshot.isGenerating,
        metadata,
        accountUsage: chatAccountQueueUsage(runtimeStore)
      });
      const submissionError = (status: typeof preflight.status): string | null => {
        if (status === "queue_full") return "You can queue up to 16 messages.";
        if (status === "text_too_large") return "Queued messages must be 32 KiB or smaller.";
        if (status === "too_many_images") {
          return "You can attach up to 10 images to a queued message.";
        }
        if (status === "image_too_large") {
          return "Each queued image must be 20 MiB or smaller.";
        }
        if (status === "document_too_large") {
          return "Queued documents must be 10 MiB or smaller.";
        }
        if (status === "queue_payload_too_large") {
          return "Queued attachments can use up to 256 MiB in total.";
        }
        if (status === "account_queue_full") {
          return "You can retain up to 64 queued chat messages across your account.";
        }
        if (status === "account_payload_too_large") {
          return "Chat drafts and queued attachments can use up to 256 MiB across your account.";
        }
        if (status === "processing") return "Wait for the document to finish processing.";
        if (status === "missing_edit") return "That queued message is no longer available.";
        return null;
      };

      if (startSnapshot.isGenerating) {
        runtimeStore.update(runtimeKey, (snapshot) => {
          const composer = chatComposerWithInputOverride(snapshot.composer, overrideInput);
          const plan = planChatComposerSubmission({
            composer,
            hasActiveRun: true,
            metadata,
            accountUsage: chatAccountQueueUsage(runtimeStore)
          });
          if (plan.status === "queued" || plan.status === "updated") {
            return { ...snapshot, composer: plan.composer, error: null };
          }
          return { ...snapshot, error: submissionError(plan.status) ?? snapshot.error };
        });
        return;
      }

      if (preflight.status !== "start") {
        const message = submissionError(preflight.status);
        if (message) setErrorForKey(runtimeKey, message);
        return;
      }

      const run = runtimeStore.beginRun(runtimeKey, {
        groupId:
          startSnapshot.conversation?.project_id ??
          preflight.item.draftProjectId ??
          selectedProjectId ??
          null
      });
      type ChatQueueTurn = {
        item: ChatQueuedMessage;
        recoverOnFailure: boolean;
        previousLastSeenItemId: string | undefined;
      };
      let currentTurn: ChatQueueTurn | undefined;
      const started = runtimeStore.updateForRun(runtimeKey, run.token, (snapshot) => {
        const composer = chatComposerWithInputOverride(snapshot.composer, overrideInput);
        const plan = planChatComposerSubmission({
          composer,
          hasActiveRun: false,
          metadata,
          accountUsage: chatAccountQueueUsage(runtimeStore)
        });
        if (plan.status !== "start") return snapshot;
        currentTurn = {
          item: plan.item,
          recoverOnFailure: plan.recoverOnFailure,
          previousLastSeenItemId: snapshot.lastSeenItemId
        };
        return {
          ...snapshot,
          composer: plan.composer,
          messages: mergeMessagesById(snapshot.messages as Message[], [
            promotedChatUserMessage(plan.item)
          ]),
          lastSeenItemId: plan.item.messageId,
          error: null
        };
      });
      if (!started || !currentTurn) {
        runtimeStore.finishRun(runtimeKey, run.token);
        return;
      }
      registerChatOptimisticMessage(runtimeStore, run.token, currentTurn.item.messageId);
      if (
        !isCompactLayout &&
        startSnapshot.messages.length === 0 &&
        isRuntimeSelected(runtimeKey)
      ) {
        // The first optimistic row swaps the centered composer for the bottom
        // composer. Restore focus once after that remount so follow-ups can be
        // queued immediately without focusing on every streamed item.
        requestAnimationFrame(() => textareaRef.current?.focus());
      }

      let conversationId =
        startSnapshot.conversation?.id ?? conversationIdFromChatRuntimeKey(runtimeKey);
      let completedAnyTurn = false;
      let stopOwnsSettlement = false;
      let detachedResponseOwnsSettlement = false;

      const scheduleBillingRefresh = () => {
        const timeout = setTimeout(() => {
          void queryClient.invalidateQueries({ queryKey: ["billingStatus"] });
          billingRefreshTimeoutsRef.current.delete(timeout);
        }, 3000);
        billingRefreshTimeoutsRef.current.add(timeout);
      };

      const sendTurn = async (turn: ChatQueueTurn): Promise<boolean> => {
        const { item, recoverOnFailure, previousLastSeenItemId } = turn;
        const localMessageId = item.messageId;
        let ownsObjectUrls = true;
        let responseRequestStarted = false;
        let conversationCreateInFlight = false;
        let turnRestored = false;
        let preserveUnresolvedResponseOwnership = false;

        const releaseObjectUrls = () => {
          if (!ownsObjectUrls) return;
          ownsObjectUrls = false;
          const snapshot = runtimeStore.get(runtimeKey);
          const retainedObjectUrls = new Set(
            snapshot
              ? [
                  ...chatComposerObjectUrls(snapshot.composer),
                  ...chatTranscriptObjectUrls(snapshot.messages as Message[])
                ]
              : []
          );
          revokeQueuedChatMessageObjectUrls(item, retainedObjectUrls);
        };

        const restoreTurn = (message: string) => {
          if (turnRestored) return true;
          let transferred = false;
          const restored = runtimeStore.updateForRun(runtimeKey, run.token, (snapshot) => {
            const recovery = recoverDetachedChatComposerDraft(snapshot.composer, item);
            let composer = recovery.composer;
            if (!recoverOnFailure && recovery.status === "restored") {
              composer = {
                ...snapshot.composer,
                queue: {
                  ...snapshot.composer.queue,
                  items: [item, ...snapshot.composer.queue.items]
                }
              };
            }
            transferred = true;
            return {
              ...snapshot,
              messages: (snapshot.messages as Message[]).filter(
                (messageItem) => messageItem.id !== localMessageId
              ),
              lastSeenItemId:
                snapshot.lastSeenItemId === localMessageId
                  ? previousLastSeenItemId
                  : snapshot.lastSeenItemId,
              composer,
              error: message
            };
          });
          if (restored && transferred) {
            ownsObjectUrls = false;
            turnRestored = true;
          }
          return restored && transferred;
        };

        const markTurnIncomplete = (message: string) => {
          runtimeStore.updateForRun(runtimeKey, run.token, (snapshot) => ({
            ...snapshot,
            messages: markOptimisticMessageIncomplete(
              snapshot.messages as Message[],
              localMessageId
            ),
            lastSeenItemId: chatCursorAfterSendFailure({
              currentCursor: snapshot.lastSeenItemId,
              optimisticMessageId: localMessageId,
              previousCursor: previousLastSeenItemId,
              responseCreated: Boolean(snapshot.currentResponseId)
            }),
            error: message
          }));
        };

        const unregisterCurrentTurn = registerChatCurrentTurn(runtimeStore, run.token, {
          responseRequestStarted: () => responseRequestStarted,
          serverRequestInFlight: () => conversationCreateInFlight,
          restoreBeforeRequest: restoreTurn,
          retainedPayload: item,
          retainsPayload: () => !turnRestored,
          countsTowardQueueLimit: true
        });

        const messageContent = queuedChatMessageContent(item, () => undefined);
        const createResponseStream = async () => {
          if (
            !runtimeStore.updateForRun(runtimeKey, run.token, (snapshot) => ({
              ...snapshot,
              assistantStreaming: false
            }))
          ) {
            return null;
          }
          registerChatOptimisticMessage(runtimeStore, run.token, localMessageId);
          const responseParams: ResponseCreateParamsStreaming = {
            conversation: conversationId!,
            model: item.model,
            input: [{ role: "user", content: messageContent }],
            metadata: { internal_message_id: localMessageId },
            stream: true,
            store: true,
            ...(item.webSearchEnabled && { tools: [{ type: "web_search" }] })
          };
          const stream = await withInferenceCapacityRetry(
            (maxInferenceSends) => {
              registerUnresolvedChatResponseMessage(runtimeStore, run.token, localMessageId);
              responseRequestStarted = true;
              return openai.responses.create(responseParams, {
                signal: run.signal,
                headers: {
                  [OPEN_SECRET_INFERENCE_SEND_LIMIT_HEADER]: String(maxInferenceSends)
                }
              });
            },
            run.signal,
            (capacityError) => {
              // Capacity rejection proves this turn was not accepted. Stop or
              // deletion can safely restore it and abort the replay delay.
              responseRequestStarted = false;
              clearUnresolvedChatResponseMessage(runtimeStore, run.token, localMessageId);
              if (
                isChatRunQueueHaltRequested(runtimeStore, run.token) ||
                isChatRuntimeDeletionPending(runtimeStore, runtimeKey)
              ) {
                throw capacityError;
              }
            }
          );

          if (!runtimeStore.setAssistantStreaming(runtimeKey, run.token, true)) return null;
          try {
            return await processStreamingResponse(
              stream,
              runtimeKey,
              run.token,
              localMessageId,
              async (responseId) => {
                if (!isRuntimeStopping(run.token)) return false;
                return cancelKnownChatResponse(runtimeKey, run.token, responseId, localMessageId);
              }
            );
          } finally {
            runtimeStore.setAssistantStreaming(runtimeKey, run.token, false);
          }
        };

        try {
          const dataUrls = new Map<File, string>();
          for (const file of item.draftImages) {
            try {
              dataUrls.set(file, await fileToDataURL(file));
            } catch (error) {
              console.error("Failed to convert image:", error);
              const restored = restoreTurn(
                "An image could not be prepared. Your message and images were restored; please try again."
              );
              if (!restored && runtimeStore.get(runtimeKey)) {
                const durableContent = queuedChatMessageContent(item, (image) =>
                  dataUrls.get(image)
                );
                runtimeStore.update(runtimeKey, (snapshot) => ({
                  ...snapshot,
                  messages: markOptimisticMessageIncomplete(
                    updateMessageById(snapshot.messages as Message[], localMessageId, (message) =>
                      message.type === "message"
                        ? ({ ...message, content: durableContent } as unknown as Message)
                        : message
                    ),
                    localMessageId
                  )
                }));
              }
              return false;
            }
          }
          messageContent.push(
            ...queuedChatMessageContent({ ...item, text: "", documentText: "" }, (file) =>
              dataUrls.get(file)
            )
          );

          const updateMaterializedMessage = (messages: readonly Message[]) =>
            updateMessageById(messages as Message[], localMessageId, (message) =>
              message.type === "message"
                ? ({ ...message, content: messageContent } as unknown as Message)
                : message
            );
          if (!runtimeStore.isRunCurrent(runtimeKey, run.token)) {
            if (runtimeStore.get(runtimeKey)) {
              runtimeStore.update(runtimeKey, (snapshot) => ({
                ...snapshot,
                messages: updateMaterializedMessage(snapshot.messages as Message[])
              }));
            }
            return false;
          }
          if (
            !runtimeStore.updateForRun(runtimeKey, run.token, (snapshot) => ({
              ...snapshot,
              messages: updateMaterializedMessage(snapshot.messages as Message[])
            }))
          ) {
            return false;
          }
          if (isChatRuntimeDeletionPending(runtimeStore, runtimeKey)) {
            restoreTurn("Sending paused because this conversation is being deleted.");
            return false;
          }

          if (!conversationId) {
            const createParams: Parameters<typeof openai.conversations.create>[0] & {
              project_id?: string;
            } = {
              metadata: {},
              ...(item.draftProjectId && { project_id: item.draftProjectId })
            };
            conversationCreateInFlight = true;
            let newConv: Awaited<ReturnType<typeof openai.conversations.create>>;
            try {
              newConv = await openai.conversations.create(createParams, {
                signal: run.signal
              });
            } finally {
              conversationCreateInFlight = false;
            }
            conversationId = newConv.id;
            const sourceWasSelected = isRuntimeSelected(runtimeKey);
            const destinationKey = createConversationChatKey(conversationId);
            const destinationSnapshot = runtimeStore.get(destinationKey);
            const rawRecordingOwnerKey = recordingOwnerKeyRef.current;
            const canonicalRecordingOwnerKey = rawRecordingOwnerKey
              ? runtimeStore.resolveKey(rawRecordingOwnerKey)
              : null;
            if (!canAdoptRecordingDestination(destinationKey, canonicalRecordingOwnerKey)) {
              restoreTurn(
                "This conversation is still processing a voice message. Your message was restored in its original draft."
              );
              return false;
            }
            if (!canAdoptAttachmentDestination(destinationSnapshot)) {
              restoreTurn(
                "This conversation is still processing an attachment. Your message was restored in its original draft."
              );
              return false;
            }

            let displacedObjectUrls: string[] = [];
            const migration = runtimeStore.rekeyRunAdoptingIdleDestination(
              runtimeKey,
              destinationKey,
              run.token,
              (source, destination) => {
                const mergedComposer = mergeChatComposerDraftsForRekey(
                  source.composer,
                  destination.composer,
                  destinationKey
                );
                displacedObjectUrls = mergedComposer.displacedObjectUrls;
                return {
                  ...source,
                  conversation: destination.conversation ?? source.conversation,
                  messages: mergeLoadedMessagesWithRuntime(
                    destination.messages as Message[],
                    source.messages
                  ),
                  composer: mergedComposer.composer,
                  error: source.error ?? destination.error,
                  lastSeenItemId: source.lastSeenItemId ?? destination.lastSeenItemId,
                  historyLoaded: source.historyLoaded || destination.historyLoaded
                };
              }
            );

            if (migration.status === "source_stale") return false;
            if (migration.status === "destination_active") {
              restoreTurn(
                "This conversation became active before your message was sent. Your message was restored in its original draft."
              );
              return false;
            }
            for (const url of displacedObjectUrls) URL.revokeObjectURL(url);

            runtimeKey = migration.key;
            runtimeStore.updateForRun(runtimeKey, run.token, (snapshot) => ({
              ...snapshot,
              conversation: newConv as Conversation,
              composer: { ...snapshot.composer, draftProjectId: null }
            }));

            const keepSelection = shouldProjectMigratedConversation(
              runtimeStore.isChatVisible(runtimeKey),
              sourceWasSelected,
              migration.destinationWasSelected
            );
            if (keepSelection) {
              activeRuntimeKeyRef.current = runtimeKey;
              setActiveRuntimeKey(runtimeKey);
              setChatId(conversationId);
              canonicalizeConversationHistoryEntry(conversationId);
            }
            window.dispatchEvent(new Event("conversationcreated"));
          }

          if (isChatRuntimeDeletionPending(runtimeStore, runtimeKey)) {
            restoreTurn("Sending paused because this conversation is being deleted.");
            return false;
          }

          const terminalState = await createResponseStream();
          scheduleBillingRefresh();
          return terminalState === "completed";
        } catch (error) {
          console.error("Failed to send message:", error);
          let errorMessage = error instanceof Error ? error.message : "Something went wrong";
          const causeMessage = (error as Error & { cause?: { message?: string } })?.cause?.message;
          if (causeMessage?.includes("Request failed with status")) errorMessage = causeMessage;

          const retainAmbiguousResponseOwnership = (message: string) => {
            markTurnIncomplete(message);
            preserveUnresolvedResponseOwnership = !runtimeStore.get(runtimeKey)?.currentResponseId;
            detachedResponseOwnsSettlement = true;
          };

          if (isChatAccountCredentialMismatchError(error)) {
            if (!responseRequestStarted || isChatRequestDefinitelyNotDispatchedError(error)) {
              restoreTurn(
                "Sending paused because the authenticated account changed. Your message was restored."
              );
            } else {
              retainAmbiguousResponseOwnership(
                "The authenticated account changed after sending began. The response may still be running; queued messages were kept."
              );
            }
            return false;
          }

          if (isImageDescriptionUnavailableError(error)) {
            restoreTurn(
              "Image description is temporarily unavailable. Your message and images were restored; please try again."
            );
            return false;
          }

          if (findOpenSecretInferenceCapacityError(error)) {
            responseRequestStarted = false;
            stoppingRuntimeRegistry.delete(runtimeKey, run.token);
            restoreTurn(
              "Inference capacity is temporarily unavailable. Your message was restored. Please try again shortly."
            );
            return false;
          }

          const parseStatusError = (status: number) => {
            if (!errorMessage.includes(`Request failed with status ${status}:`)) return null;
            try {
              const jsonMatch = errorMessage.match(
                new RegExp(`Request failed with status ${status}:\\s*({.*})`)
              );
              return jsonMatch?.[1]
                ? (JSON.parse(jsonMatch[1]) as { status: number; message: string })
                : null;
            } catch (parseError) {
              console.error(`Failed to parse ${status} error:`, parseError);
              return null;
            }
          };

          const status413Error = parseStatusError(413);
          if (status413Error?.message === "Message exceeds context limit") {
            restoreTurn("Your message exceeds the context limit for this model.");
            if (isRuntimeSelected(runtimeKey)) setContextLimitDialogOpen(true);
            return false;
          }

          const status403Error = parseStatusError(403);
          if (status403Error) {
            let displayError: string;
            if (status403Error.message === "Free tier token limit exceeded") {
              displayError =
                "This conversation is too long for the free tier. Upgrade to Pro for longer conversations.";
              if (isRuntimeSelected(runtimeKey)) {
                setUpgradeFeature("tokens");
                setUpgradeDialogOpen(true);
              }
            } else if (status403Error.message === "Usage limit reached") {
              const isFreeTier =
                !billingStatus?.product_name || billingStatus.product_name.toLowerCase() === "free";
              if (isFreeTier) {
                displayError =
                  "You've reached your daily usage limit. Upgrade to Pro for more chats.";
              } else {
                const isPro =
                  billingStatus.product_name?.toLowerCase().includes("pro") &&
                  !billingStatus.product_name?.toLowerCase().includes("max");
                displayError = isPro
                  ? "You've reached your monthly Pro limit. Upgrade to Max for 10x more usage."
                  : "You've reached your monthly usage limit. Please wait for the next billing cycle.";
              }
              if (isRuntimeSelected(runtimeKey)) {
                setUpgradeFeature("usage");
                setUpgradeDialogOpen(true);
              }
            } else {
              displayError =
                status403Error.message || "Access denied. Please check your subscription.";
            }
            restoreTurn(displayError);
            return false;
          }

          if (
            isChatRequestDefinitelyNotDispatchedError(error) ||
            isChatResponseDefinitelyRejectedError(error)
          ) {
            restoreTurn(`${errorMessage}. Please try again.`);
            return false;
          }

          if (error instanceof Error && error.name !== "AbortError") {
            if (!responseRequestStarted) {
              restoreTurn(`${errorMessage}. Please try again.`);
            } else {
              // Once an ambiguous streaming POST has started, replaying it can
              // create a duplicate provider turn. Keep later FIFO items staged
              // and retain known server ownership until polling or Stop reaches
              // a terminal response state.
              retainAmbiguousResponseOwnership(
                `${errorMessage}. The response may still be running; queued messages were kept.`
              );
            }
          }
          return false;
        } finally {
          unregisterCurrentTurn();
          unregisterChatOptimisticMessage(runtimeStore, run.token, localMessageId);
          if (!preserveUnresolvedResponseOwnership) {
            clearUnresolvedChatResponseMessage(runtimeStore, run.token, localMessageId);
          }
          releaseObjectUrls();
        }
      };

      try {
        while (currentTurn && runtimeStore.isRunCurrent(runtimeKey, run.token)) {
          const completed = await sendTurn(currentTurn);
          if (!completed || !runtimeStore.isRunCurrent(runtimeKey, run.token)) {
            if (runtimeStore.isRunCurrent(runtimeKey, run.token) && isRuntimeStopping(run.token)) {
              const responseId = runtimeStore.get(runtimeKey)?.currentResponseId;
              stopOwnsSettlement = Boolean(responseId);
              if (!responseId) {
                // A failed POST before response.created can still be accepted
                // server-side. Keep the run and Stop intent fenced until item
                // polling recovers its response UUID; never overlap the FIFO.
                setErrorForKey(
                  runtimeKey,
                  "Maple could not confirm that the response stopped. Queued messages were kept."
                );
              }
            }
            break;
          }
          completedAnyTurn = true;
          if (isChatRunQueueHaltRequested(runtimeStore, run.token)) {
            if (!runtimeStore.get(runtimeKey)?.currentResponseId) {
              stoppingRuntimeRegistry.delete(runtimeKey, run.token);
            }
            break;
          }
          if (isRuntimeStopping(run.token)) break;
          if (isChatRuntimeDeletionPending(runtimeStore, runtimeKey)) break;

          let nextTurn: ChatQueueTurn | undefined;
          const advanced = runtimeStore.updateForRun(runtimeKey, run.token, (snapshot) => {
            const next = takeNextChatQueuedMessage(snapshot.composer.queue);
            if (next.status !== "taken") {
              return {
                ...snapshot,
                currentResponseId: undefined,
                assistantStreaming: false
              };
            }
            nextTurn = {
              item: next.item,
              recoverOnFailure: false,
              previousLastSeenItemId: snapshot.lastSeenItemId
            };
            return {
              ...snapshot,
              composer: { ...snapshot.composer, queue: next.queue },
              messages: mergeMessagesById(snapshot.messages as Message[], [
                promotedChatUserMessage(next.item)
              ]),
              lastSeenItemId: next.item.messageId,
              currentResponseId: undefined,
              assistantStreaming: false,
              error: null
            };
          });
          if (!advanced) break;
          if (!nextTurn) {
            break;
          }
          currentTurn = nextTurn;
          registerChatOptimisticMessage(runtimeStore, run.token, currentTurn.item.messageId);
        }
      } finally {
        if (
          runtimeStore.isRunCurrent(runtimeKey, run.token) &&
          !stopOwnsSettlement &&
          !detachedResponseOwnsSettlement
        ) {
          if (completedAnyTurn) runtimeStore.completeRun(runtimeKey, run.token);
          else runtimeStore.finishRun(runtimeKey, run.token);
        }
        if (!runtimeStore.isRunCurrent(runtimeKey, run.token)) {
          clearUnresolvedChatResponseMessage(runtimeStore, run.token);
        }
        if (!runtimeStore.isRunCurrent(runtimeKey, run.token)) {
          stoppingRuntimeRegistry.delete(runtimeKey, run.token);
        }
        if (!runtimeStore.isRunCurrent(runtimeKey, run.token)) {
          clearChatRunQueueHalt(runtimeStore, run.token);
        }
      }
    },
    [
      billingRefreshTimeoutsRef,
      billingStatus,
      cancelKnownChatResponse,
      isRuntimeSelected,
      isCompactLayout,
      isWebSearchEnabled,
      model,
      openai,
      processStreamingResponse,
      queryClient,
      runtimeStore,
      selectedProjectId,
      setErrorForKey,
      stoppingRuntimeRegistry
    ]
  );

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    // On desktop: Enter submits, Shift+Enter for new line
    // On mobile: Enter for new line, no keyboard shortcut to submit (use button)
    if (e.nativeEvent.isComposing) return;
    if (e.key === "Escape" && queueEdit) {
      e.preventDefault();
      discardQueueEdit();
      return;
    }
    if ((e.shiftKey || isCompactLayout) && continueChatComposerList(e, setInput)) {
      return;
    }
    if (e.key === "Enter" && !e.shiftKey && !isCompactLayout) {
      e.preventDefault();
      handleSendMessage();
    }
  };

  const handleBeforeInput = (event: React.FormEvent<HTMLTextAreaElement>) => {
    continueChatComposerListBeforeInput(event, setInput);
  };

  return (
    <ResizableSidebarLayout
      data-runtime-key={runtimeStore.resolveKey(activeRuntimeKey)}
      data-generating={isGenerating ? "true" : "false"}
      isCompactLayout={isCompactLayout}
      isOpen={isSidebarOpen}
      mode="chat"
      onOpenChange={setIsSidebarOpen}
      onTransitionChange={setIsSidebarTransitioning}
      sidebar={<Sidebar chatId={chatId} isOpen={isSidebarOpen} onToggle={toggleSidebar} />}
      userId={os.auth.user?.user.id}
    >
      {/* Main Content */}
      <div className="flex flex-col flex-1 min-w-0 min-h-0 bg-background overflow-hidden relative">
        {/* Error message - fixed at top below header, always visible */}
        {error && (
          <div className={CHAT_ALERT_CLASS}>
            <Alert variant="destructive" className="bg-background">
              <AlertCircle className="h-4 w-4" />
              <AlertDescription>{error}</AlertDescription>
            </Alert>
          </div>
        )}

        {/* TTS playback error - shows when audio context is unavailable (e.g., Lockdown Mode) */}
        {playbackError && (
          <div className={CHAT_ALERT_CLASS}>
            <Alert variant="destructive" className="bg-background">
              <AlertCircle className="h-4 w-4" />
              <AlertDescription className="flex items-center justify-between">
                <span>{playbackError}</span>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-6 px-2 ml-2"
                  onClick={clearPlaybackError}
                  aria-label="Dismiss TTS error"
                >
                  <X className="h-4 w-4" />
                </Button>
              </AlertDescription>
            </Alert>
          </div>
        )}

        {/* Sidebar toggle + wordmark — fixed except on compact layouts while chatting (two-row header below) */}
        {!isSidebarOpen && !isSidebarTransitioning && !(isCompactLayout && messages.length > 0) && (
          <div className="fixed left-4 top-[9.5px] z-20 flex items-center gap-1.5">
            <SidebarToggle onToggle={toggleSidebar} />
            <MapleWordmark
              className="h-4 w-auto animate-in fade-in-0 slide-in-from-left-1 duration-300"
              aria-hidden
            />
          </div>
        )}

        {/* Only show header when there are messages (conversation exists) */}
        {messages.length > 0 &&
          (isLandscapeMobile && !isSidebarOpen ? (
            <div className="z-10 flex shrink-0 items-center gap-2 bg-background px-1 py-1 pr-4">
              <SidebarToggle onToggle={toggleSidebar} />
              <div className="min-w-0 overflow-hidden">
                <MapleWordmark className="h-4 w-auto max-w-full" aria-hidden />
              </div>
              <h1
                className={`min-w-0 flex-1 truncate px-1 text-center text-base font-medium text-foreground transition-colors duration-300 ${
                  titleJustUpdated ? "title-update-animation" : ""
                }`}
              >
                {conversation?.metadata?.title || "Chat"}
              </h1>
              <Button
                variant="outline"
                size="icon"
                className="h-9 w-9 shrink-0 border-0"
                onClick={handleNewChatFromHeader}
                aria-label="New chat"
              >
                <SquarePen className="h-4 w-4" />
              </Button>
            </div>
          ) : isMobile && !isSidebarOpen ? (
            <div className="z-10 flex shrink-0 flex-col gap-2 bg-background pb-2 pl-1 pr-4 pt-2">
              <div className="flex items-center justify-between gap-3">
                <div className="flex min-w-0 flex-1 items-center gap-1.5">
                  <SidebarToggle onToggle={toggleSidebar} />
                  <div className="min-w-0 overflow-hidden">
                    <MapleWordmark className="h-4 w-auto max-w-full" aria-hidden />
                  </div>
                </div>
                <Button
                  variant="outline"
                  size="icon"
                  className="h-9 w-9 shrink-0 border-0"
                  onClick={handleNewChatFromHeader}
                  aria-label="New chat"
                >
                  <SquarePen className="h-4 w-4" />
                </Button>
              </div>
              <h1
                className={`w-full truncate px-1 text-center text-base font-medium text-foreground transition-colors duration-300 ${
                  titleJustUpdated ? "title-update-animation" : ""
                }`}
              >
                {conversation?.metadata?.title || "Chat"}
              </h1>
            </div>
          ) : (
            <ChatDesktopConversationHeader
              title={conversation?.metadata?.title || "Chat"}
              titleClassName={titleJustUpdated ? "title-update-animation" : undefined}
              isSidebarOpen={isSidebarOpen}
              onNewChat={handleNewChatFromHeader}
            />
          ))}

        {/* Messages Area */}
        <div
          ref={chatContainerRef}
          data-testid="chat-scroll-container"
          className="flex-1 min-h-0 overflow-y-auto overscroll-y-none flex flex-col relative"
        >
          {isLoadingOlderMessages && (
            <div className="pointer-events-none absolute inset-x-0 top-0 z-10 flex items-center justify-center py-4">
              <div className="flex items-center gap-2 text-sm text-muted-foreground">
                <div className="w-2 h-2 bg-foreground/60 rounded-full animate-pulse" />
                <div className="w-2 h-2 bg-foreground/60 rounded-full animate-pulse delay-75" />
                <div className="w-2 h-2 bg-foreground/60 rounded-full animate-pulse delay-150" />
              </div>
            </div>
          )}

          {/* Only show messages when there are messages */}
          {messages.length > 0 && (
            <div className="mx-auto flex min-h-full w-full max-w-4xl flex-col p-4 md:p-6 landscape-short:p-2">
              <div
                ref={historyTopSentinelRef}
                data-testid="chat-history-top"
                className="h-px w-full"
                aria-hidden="true"
              />

              {/* Message list with modern ChatGPT/Claude style */}
              <div className="space-y-1">
                <MessageList messages={messages} isGenerating={isGenerating} chatId={chatId} />
              </div>

              <div ref={messagesEndRef} />
              <div
                ref={historyBottomCompensationRef}
                data-testid="chat-history-bottom-compensation"
                className="shrink-0"
                aria-hidden="true"
              />
            </div>
          )}
        </div>

        {/* Input Area - centered when no messages, fixed at bottom when chatting */}
        {messages.length === 0 && !chatId ? (
          // Centered input for new chat
          <div
            className={`absolute inset-0 flex flex-col px-4 ${
              isFullscreenAnimating ? "transition-all duration-300" : ""
            } ${isFullscreen ? "justify-start pt-8" : "justify-center"}`}
          >
            <div
              className={`mx-auto w-full ${
                isFullscreenAnimating ? "transition-all duration-300" : ""
              } ${isFullscreen ? "flex h-full max-w-6xl flex-col" : "max-w-[650px]"}`}
            >
              {!isFullscreen && <div className="mb-16 landscape-short:mb-4" />}

              <div
                className={`flex flex-col items-center gap-6 landscape-short:gap-3 ${isFullscreen ? "flex-1 justify-center" : ""}`}
              >
                {!isFullscreen && (
                  <h1 className="mb-6 landscape-short:mb-2 w-full overflow-visible pb-1 text-center font-displayWide text-4xl landscape-short:text-2xl font-normal leading-tight brand-gradient-text sm:leading-relaxed">
                    Research anything...
                  </h1>
                )}

                <form onSubmit={handleSendMessage} className="relative w-full">
                  <div className="space-y-2">
                    {!queueEdit && (draftImages.length > 0 || documentName) && (
                      <div className="space-y-2">
                        {draftImages.length > 0 && (
                          <div className="flex flex-wrap gap-2">
                            {draftImages.map((file, i) => (
                              <div key={i} className="group relative">
                                <img
                                  src={imageUrls.get(file) || ""}
                                  alt={`Attachment ${i + 1}`}
                                  className="h-16 w-16 rounded-xl border object-cover"
                                />
                                <button
                                  type="button"
                                  onClick={() => removeImage(i)}
                                  aria-label={`Remove attachment ${i + 1}`}
                                  className="absolute -right-1 -top-1 rounded-full border bg-background p-0.5 opacity-0 transition-opacity group-hover:opacity-100 disabled:pointer-events-none disabled:opacity-40"
                                >
                                  <X className="h-3 w-3" />
                                </button>
                              </div>
                            ))}
                          </div>
                        )}

                        {documentName && (
                          <div className="flex items-center gap-2 rounded-2xl bg-muted/50 p-2">
                            <FileText className="h-4 w-4 text-muted-foreground" />
                            <span className="flex-1 truncate text-sm">{documentName}</span>
                            <button
                              type="button"
                              onClick={removeDocument}
                              aria-label="Remove document"
                              className="text-muted-foreground hover:text-foreground disabled:pointer-events-none disabled:opacity-40"
                            >
                              <X className="h-3 w-3" />
                            </button>
                          </div>
                        )}
                      </div>
                    )}

                    {(attachmentError || audioError) && (
                      <div className="px-2 text-sm text-maple-error">
                        {attachmentError || audioError}
                      </div>
                    )}

                    <div
                      className={`relative flex flex-col overflow-hidden rounded-3xl border border-[hsl(var(--maple-secondary-container))] bg-background focus-within:border-[hsl(var(--maple-primary))] ${
                        isFullscreenAnimating ? "transition-all duration-300" : "transition-colors"
                      } ${isFullscreen ? "h-[70vh] max-h-[800px] min-h-0" : ""}`}
                    >
                      <QueuedComposerMessages
                        items={queuedMessages}
                        className="pr-10"
                        editingQueueId={queueEdit?.queueId}
                        getFallbackLabel={queuedChatMessageFallbackLabel}
                        onRemove={cancelQueuedMessage}
                        onEdit={editQueuedMessage}
                      />
                      <button
                        type="button"
                        onClick={toggleFullscreen}
                        className="absolute right-2 top-2 z-10 rounded-full p-1.5 text-muted-foreground/60 transition-colors hover:bg-muted/50 hover:text-foreground"
                        aria-label={isFullscreen ? "Exit fullscreen" : "Enter fullscreen"}
                      >
                        {isFullscreen ? (
                          <Shrink className="h-4 w-4" />
                        ) : (
                          <Expand className="h-4 w-4" />
                        )}
                      </button>
                      <Textarea
                        ref={textareaRef}
                        value={input}
                        onChange={(e) => setInput(e.target.value)}
                        onKeyDown={handleKeyDown}
                        onBeforeInput={handleBeforeInput}
                        onPaste={handlePaste}
                        placeholder={
                          queueEdit ? QUEUED_MESSAGE_EDIT_PLACEHOLDER : "Message Maple..."
                        }
                        disabled={isRecordingForActive}
                        className={`resize-none border-0 bg-transparent pl-4 pr-8 text-base leading-6 focus-visible:ring-0 focus-visible:ring-offset-0 placeholder:text-muted-foreground/60 ${
                          isFullscreen
                            ? "flex-1 min-h-0 pt-3 pb-2"
                            : "min-h-[52px] max-h-[200px] pt-3 pb-2"
                        }`}
                        rows={isFullscreen ? undefined : 1}
                        id="message"
                      />

                      <div className="grid shrink-0 grid-cols-[minmax(0,1fr)_auto] items-end gap-x-2 gap-y-2 px-2 pb-2 pt-1">
                        <div className="flex min-w-0 flex-wrap items-center gap-1.5 sm:gap-2">
                          <ModelSelector />

                          <ConversationProjectPicker
                            selectedProjectId={draftProjectId}
                            onSelect={setDraftProjectId}
                            disabled={isGenerating}
                          />

                          <Button
                            type="button"
                            variant="ghost"
                            size="sm"
                            pressScale={false}
                            className="group h-8 w-8 p-0 text-[hsl(var(--maple-secondary-700))] hover:bg-[hsl(var(--maple-primary-container))] hover:text-[hsl(var(--maple-secondary-700))]"
                            onClick={() => {
                              const newValue = !isWebSearchEnabled;
                              setIsWebSearchEnabled(newValue);
                              localStorage.setItem("webSearchEnabled", newValue.toString());
                            }}
                            aria-label={
                              isWebSearchEnabled ? "Disable web search" : "Enable web search"
                            }
                          >
                            <Globe
                              className={`h-4 w-4 will-change-transform transition-transform duration-150 ease-out group-active:scale-90 motion-reduce:transition-none ${
                                isWebSearchEnabled
                                  ? "text-[hsl(var(--maple-primary))]"
                                  : "text-[hsl(var(--maple-secondary-700))]"
                              }`}
                            />
                          </Button>

                          <DropdownMenu>
                            <DropdownMenuTrigger asChild>
                              <Button
                                type="button"
                                variant="ghost"
                                size="sm"
                                pressScale={false}
                                className="group h-8 w-8 p-0 text-[hsl(var(--maple-secondary-700))] hover:bg-[hsl(var(--maple-primary-container))] hover:text-[hsl(var(--maple-secondary-700))]"
                                disabled={Boolean(queueEdit) || isProcessingDocument}
                                aria-busy={isProcessingDocument}
                                aria-label={
                                  isProcessingDocument
                                    ? "Processing document"
                                    : queueEdit
                                      ? "Attachments unavailable while editing a queued message"
                                      : "Add attachment"
                                }
                              >
                                {isProcessingDocument ? (
                                  <Loader2
                                    className="h-4 w-4 animate-spin text-[hsl(var(--maple-secondary-700))]"
                                    aria-hidden="true"
                                  />
                                ) : (
                                  <Plus
                                    className="h-4 w-4 text-[hsl(var(--maple-secondary-700))] will-change-transform transition-transform duration-150 ease-out group-active:scale-90 motion-reduce:transition-none"
                                    aria-hidden="true"
                                  />
                                )}
                              </Button>
                            </DropdownMenuTrigger>
                            <DropdownMenuContent align="start">
                              <DropdownMenuItem
                                disabled={Boolean(queueEdit)}
                                onClick={() => {
                                  if (!canUseImages) {
                                    setUpgradeFeature("image");
                                    setUpgradeDialogOpen(true);
                                  } else {
                                    fileInputOwnerKeyRef.current = activeRuntimeKeyRef.current;
                                    fileInputRef.current?.click();
                                  }
                                }}
                              >
                                <Image className="mr-2 h-4 w-4" />
                                <span>Add Images</span>
                              </DropdownMenuItem>
                              <DropdownMenuItem
                                disabled={Boolean(queueEdit)}
                                onClick={() => {
                                  if (!isTauriEnv) {
                                    setDocumentPlatformDialogOpen(true);
                                  } else if (!canUseDocuments) {
                                    setUpgradeFeature("document");
                                    setUpgradeDialogOpen(true);
                                  } else {
                                    documentInputOwnerKeyRef.current = activeRuntimeKeyRef.current;
                                    documentInputRef.current?.click();
                                  }
                                }}
                              >
                                <FileText className="mr-2 h-4 w-4" />
                                <span>Add Document</span>
                              </DropdownMenuItem>
                            </DropdownMenuContent>
                          </DropdownMenu>
                        </div>

                        <div className="flex shrink-0 items-center self-end gap-1.5 sm:gap-2">
                          <Button
                            type="button"
                            onClick={startRecording}
                            disabled={isStopping || isRecording || !canUseVoice}
                            size="icon"
                            variant="ghost"
                            className="h-8 w-8 rounded-xl hover:bg-muted sm:h-9 sm:w-9"
                          >
                            <Mic className="h-4 w-4" />
                          </Button>
                          {queueEdit ? (
                            <DiscardQueuedMessageEditButton onDiscard={discardQueueEdit} />
                          ) : null}
                          {showsStop ? (
                            <Button
                              type="button"
                              onClick={handleCancelResponse}
                              disabled={isStopping}
                              aria-label="Stop generating"
                              size="icon"
                              variant="destructive"
                              className="h-8 w-8 rounded-xl sm:h-9 sm:w-9"
                            >
                              <div className="h-3 w-3 rounded-md bg-current" />
                            </Button>
                          ) : null}
                          <button
                            type="submit"
                            aria-label={queueEdit ? "Save queued message" : "Send message"}
                            disabled={!canSubmitMessage}
                            className="flex h-8 w-8 items-center justify-center rounded-full bg-gradient-to-b from-[hsl(var(--maple-primary))] to-[hsl(var(--maple-primary-strong))] text-[hsl(var(--maple-on-primary))]/90 transition-all duration-200 ease-out active:scale-[0.95] disabled:pointer-events-none disabled:opacity-40 sm:h-9 sm:w-9"
                          >
                            <ArrowUp className="h-3.5 w-3.5 sm:h-4 sm:w-4" />
                          </button>
                        </div>
                      </div>

                      {isRecordingForActive && (
                        <RecordingOverlay
                          isRecording={isRecording}
                          isProcessing={isProcessingSend || isTranscribing}
                          onSend={() => stopRecording(true)}
                          onCancel={() => stopRecording(false)}
                          isCompact={false}
                          className="absolute inset-0 rounded-3xl"
                        />
                      )}
                    </div>
                  </div>
                </form>

                {!isFullscreen && (
                  <p className="flex items-center justify-center gap-1 text-center text-xs text-muted-foreground/60">
                    <LockKeyhole className="h-3 w-3" />
                    Encrypted and private at every step
                  </p>
                )}
              </div>
            </div>
          </div>
        ) : (
          // Fixed at bottom when there are messages
          <div className="bg-background pb-[env(safe-area-inset-bottom)]">
            <div className="mx-auto max-w-4xl px-4 landscape-short:px-3">
              <form onSubmit={handleSendMessage} className="relative">
                <div className="space-y-2 landscape-short:space-y-1">
                  {!queueEdit && (draftImages.length > 0 || documentName) && (
                    <div className="space-y-2 landscape-short:space-y-1">
                      {draftImages.length > 0 && (
                        <div className="flex flex-wrap gap-2">
                          {draftImages.map((file, i) => (
                            <div key={i} className="group relative">
                              <img
                                src={imageUrls.get(file) || ""}
                                alt={`Attachment ${i + 1}`}
                                className="h-12 w-12 rounded-xl border object-cover"
                              />
                              <button
                                type="button"
                                onClick={() => removeImage(i)}
                                aria-label={`Remove attachment ${i + 1}`}
                                className="absolute -right-1 -top-1 rounded-full border bg-background p-0.5 opacity-0 transition-opacity group-hover:opacity-100 disabled:pointer-events-none disabled:opacity-40"
                              >
                                <X className="h-2.5 w-2.5" />
                              </button>
                            </div>
                          ))}
                        </div>
                      )}

                      {documentName && (
                        <div className="flex items-center gap-2 rounded-2xl bg-muted/50 p-1.5 text-xs">
                          <FileText className="h-3 w-3 text-muted-foreground" />
                          <span className="flex-1 truncate">{documentName}</span>
                          <button
                            type="button"
                            onClick={removeDocument}
                            aria-label="Remove document"
                            className="text-muted-foreground hover:text-foreground disabled:pointer-events-none disabled:opacity-40"
                          >
                            <X className="h-2.5 w-2.5" />
                          </button>
                        </div>
                      )}
                    </div>
                  )}

                  {(attachmentError || audioError) && (
                    <div className="px-2 text-xs text-maple-error">
                      {attachmentError || audioError}
                    </div>
                  )}

                  <ChatComposerSurface>
                    <QueuedComposerMessages
                      items={queuedMessages}
                      editingQueueId={queueEdit?.queueId}
                      getFallbackLabel={queuedChatMessageFallbackLabel}
                      onRemove={cancelQueuedMessage}
                      onEdit={editQueuedMessage}
                    />
                    <Textarea
                      ref={textareaRef}
                      value={input}
                      onChange={(e) => setInput(e.target.value)}
                      onKeyDown={handleKeyDown}
                      onBeforeInput={handleBeforeInput}
                      onPaste={handlePaste}
                      placeholder={queueEdit ? QUEUED_MESSAGE_EDIT_PLACEHOLDER : "Message Maple..."}
                      disabled={isRecordingForActive}
                      className={CHAT_COMPOSER_TEXTAREA_CLASS}
                      rows={1}
                      id="message"
                    />

                    <div className="grid grid-cols-[minmax(0,1fr)_auto] items-end gap-x-2 gap-y-2 px-2 pb-2 landscape-short:pb-1.5 pt-1">
                      <div className="flex min-w-0 flex-wrap items-center gap-1.5 sm:gap-2">
                        <ModelSelector />

                        <Button
                          type="button"
                          variant="ghost"
                          size="sm"
                          pressScale={false}
                          className="group h-8 w-8 p-0 text-[hsl(var(--maple-secondary-700))] hover:bg-[hsl(var(--maple-primary-container))] hover:text-[hsl(var(--maple-secondary-700))]"
                          onClick={() => {
                            const newValue = !isWebSearchEnabled;
                            setIsWebSearchEnabled(newValue);
                            localStorage.setItem("webSearchEnabled", newValue.toString());
                          }}
                          aria-label={
                            isWebSearchEnabled ? "Disable web search" : "Enable web search"
                          }
                        >
                          <Globe
                            className={`h-4 w-4 will-change-transform transition-transform duration-150 ease-out group-active:scale-90 motion-reduce:transition-none ${
                              isWebSearchEnabled
                                ? "text-[hsl(var(--maple-primary))]"
                                : "text-[hsl(var(--maple-secondary-700))]"
                            }`}
                          />
                        </Button>

                        <DropdownMenu>
                          <DropdownMenuTrigger asChild>
                            <Button
                              type="button"
                              variant="ghost"
                              size="sm"
                              pressScale={false}
                              className="group h-8 w-8 p-0 text-[hsl(var(--maple-secondary-700))] hover:bg-[hsl(var(--maple-primary-container))] hover:text-[hsl(var(--maple-secondary-700))]"
                              disabled={Boolean(queueEdit) || isProcessingDocument}
                              aria-busy={isProcessingDocument}
                              aria-label={
                                isProcessingDocument
                                  ? "Processing document"
                                  : queueEdit
                                    ? "Attachments unavailable while editing a queued message"
                                    : "Add attachment"
                              }
                            >
                              {isProcessingDocument ? (
                                <Loader2
                                  className="h-4 w-4 animate-spin text-[hsl(var(--maple-secondary-700))]"
                                  aria-hidden="true"
                                />
                              ) : (
                                <Plus
                                  className="h-4 w-4 text-[hsl(var(--maple-secondary-700))] will-change-transform transition-transform duration-150 ease-out group-active:scale-90 motion-reduce:transition-none"
                                  aria-hidden="true"
                                />
                              )}
                            </Button>
                          </DropdownMenuTrigger>
                          <DropdownMenuContent align="start">
                            <DropdownMenuItem
                              disabled={Boolean(queueEdit)}
                              onClick={() => {
                                if (!canUseImages) {
                                  setUpgradeFeature("image");
                                  setUpgradeDialogOpen(true);
                                } else {
                                  fileInputOwnerKeyRef.current = activeRuntimeKeyRef.current;
                                  fileInputRef.current?.click();
                                }
                              }}
                            >
                              <Image className="mr-2 h-4 w-4" />
                              <span>Add Images</span>
                            </DropdownMenuItem>
                            <DropdownMenuItem
                              disabled={Boolean(queueEdit)}
                              onClick={() => {
                                if (!isTauriEnv) {
                                  setDocumentPlatformDialogOpen(true);
                                } else if (!canUseDocuments) {
                                  setUpgradeFeature("document");
                                  setUpgradeDialogOpen(true);
                                } else {
                                  documentInputOwnerKeyRef.current = activeRuntimeKeyRef.current;
                                  documentInputRef.current?.click();
                                }
                              }}
                            >
                              <FileText className="mr-2 h-4 w-4" />
                              <span>Add Document</span>
                            </DropdownMenuItem>
                          </DropdownMenuContent>
                        </DropdownMenu>
                      </div>

                      <div className="flex shrink-0 items-center self-end gap-1.5 sm:gap-2">
                        <Button
                          type="button"
                          onClick={startRecording}
                          disabled={isStopping || isRecording || !canUseVoice}
                          size="icon"
                          variant="ghost"
                          className="h-8 w-8 rounded-xl text-[hsl(var(--maple-secondary-700))] hover:bg-[hsl(var(--maple-primary-container))] hover:text-[hsl(var(--maple-secondary-700))]"
                        >
                          <Mic className="h-4 w-4 text-[hsl(var(--maple-secondary-700))]" />
                        </Button>
                        {queueEdit ? (
                          <DiscardQueuedMessageEditButton onDiscard={discardQueueEdit} />
                        ) : null}
                        {showsStop ? (
                          <Button
                            type="button"
                            onClick={handleCancelResponse}
                            disabled={isStopping}
                            aria-label="Stop generating"
                            size="icon"
                            variant="destructive"
                            className="h-8 w-8 rounded-xl"
                          >
                            <div className="h-3 w-3 rounded-md bg-current" />
                          </Button>
                        ) : null}
                        <button
                          type="submit"
                          aria-label={queueEdit ? "Save queued message" : "Send message"}
                          disabled={!canSubmitMessage}
                          className="flex h-8 w-8 items-center justify-center rounded-full bg-gradient-to-b from-[hsl(var(--maple-primary))] to-[hsl(var(--maple-primary-strong))] text-[hsl(var(--maple-on-primary))]/90 transition-all duration-200 ease-out active:scale-[0.95] disabled:pointer-events-none disabled:opacity-40"
                        >
                          <ArrowUp className="h-4 w-4" />
                        </button>
                      </div>
                    </div>

                    {isRecordingForActive && (
                      <RecordingOverlay
                        isRecording={isRecording}
                        isProcessing={isProcessingSend || isTranscribing}
                        onSend={() => stopRecording(true)}
                        onCancel={() => stopRecording(false)}
                        isCompact={true}
                        className="absolute inset-0 rounded-3xl"
                      />
                    )}
                  </ChatComposerSurface>
                </div>
              </form>
              <p className="mb-2 mt-1 landscape-short:mb-1 text-center text-[10px] text-muted-foreground/50">
                AI can make mistakes. Check important info.
              </p>
            </div>
          </div>
        )}

        {/* Upgrade dialog for paid features and usage limits */}
        <UpgradePromptDialog
          open={upgradeDialogOpen}
          onOpenChange={setUpgradeDialogOpen}
          onStartNewChat={handleNewChatFromUpgrade}
          feature={upgradeFeature}
        />

        {/* Document platform dialog for web users */}
        <DocumentPlatformDialog
          open={documentPlatformDialogOpen}
          onOpenChange={setDocumentPlatformDialogOpen}
          hasProAccess={canUseDocuments || false}
        />

        {/* Context limit dialog for 413 errors */}
        <ContextLimitDialog
          open={contextLimitDialogOpen}
          onOpenChange={setContextLimitDialogOpen}
          currentModel={model}
          hasDocument={!!documentName}
        />

        {/* Hidden file inputs - must be outside conditional rendering to work in both views */}
        <input
          type="file"
          ref={fileInputRef}
          accept="image/jpeg,image/jpg,image/png,image/webp"
          multiple
          onChange={handleAddImages}
          className="hidden"
        />
        <input
          type="file"
          ref={documentInputRef}
          accept=".pdf,.doc,.docx,.txt,.md"
          onChange={handleDocumentUpload}
          className="hidden"
        />
      </div>
    </ResizableSidebarLayout>
  );
}
