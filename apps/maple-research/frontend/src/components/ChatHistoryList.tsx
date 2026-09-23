import { useState, useMemo, useCallback, useEffect, useRef, useSyncExternalStore } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  MoreHorizontal,
  Trash,
  Pencil,
  ChevronDown,
  ChevronRight,
  CheckSquare,
  RefreshCw,
  Folder,
  FolderPlus,
  FolderInput,
  Pin,
  PinOff,
  SquarePen,
  Loader2
} from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger
} from "@/components/ui/dropdown-menu";
import { Checkbox } from "@/components/ui/checkbox";
import { RenameChatDialog } from "@/components/RenameChatDialog";
import { DeleteChatDialog } from "@/components/DeleteChatDialog";
import { BulkDeleteDialog } from "@/components/BulkDeleteDialog";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useOpenSecret, type Conversation, type ConversationProjectListItem } from "@mapleai/sdk";
import { useRouter } from "@tanstack/react-router";
import { useSelectedProjectState } from "@/state/useLocalState";
import { ConversationProjectDialog } from "@/components/ConversationProjectDialog";
import { DeleteConversationProjectDialog } from "@/components/DeleteConversationProjectDialog";
import { MoveChatsDialog } from "@/components/MoveChatsDialog";
import { listAllConversationProjects, listAllConversations } from "@/utils/paginatedLists";
import {
  createChatHistoryEntryForDraft,
  pushChatHistoryEntryForDraft,
  type NewChatNavigationDetail
} from "@/services/chatRuntimeNavigation";
import { useChatRuntimeStore } from "@/contexts/ChatRuntimeContext";
import { useOpenAI } from "@/ai/useOpenAi";
import {
  resumeOrCreateChatDraftKey,
  rootChatDraftKeyAfterProjectDeletion
} from "@/services/chatDraftSelection";
import { createConversationChatKey } from "@/services/chatRuntimeStore";
import {
  beginAllChatRuntimeDeletionFence,
  beginChatProjectRuntimeDeletionFence,
  beginChatRuntimeDeletionFence
} from "@/services/chatRuntimeDeletionFence";
import {
  INITIAL_CHAT_HISTORY_RETRY_COUNT,
  conversationHistoryQueryKey,
  initialChatHistoryPage,
  shouldLoadConversationHistory
} from "@/services/chatHistoryAccountScope";
import {
  cancelChatResponseForHistoryDeletion,
  quiesceChatRuntimeRunsForHistoryDeletion,
  settleChatRuntimeAfterHistoryCancellation
} from "@/services/chatHistoryDeletionQuiescence";
import { assertChatAccountCredential } from "@/services/chatAccountCredential";

const MAX_PROJECTS = 10;
/** Lucide default; keep sidebar list icons visually consistent. */
const ICON_STROKE = 2;
const ROW_CONTENT_Z = "z-0";
const ROW_CHECKBOX_Z = "z-20";
const ROW_MENU_Z = "z-30";
const SIDEBAR_TITLE_CLASS =
  "min-w-0 flex-1 overflow-hidden whitespace-nowrap [mask-image:linear-gradient(to_right,black_calc(100%_-_1rem),transparent)]";
const SIDEBAR_ELLIPSIS_FADE =
  "pointer-events-none w-4 shrink-0 self-stretch bg-gradient-to-r from-transparent to-[hsl(var(--muted))] dark:to-[hsl(var(--sidebar))]";
const SIDEBAR_ELLIPSIS_TRIGGER_ROW_BASE = `absolute inset-y-0 right-0 ${ROW_MENU_Z} flex min-h-0 items-stretch`;

/** Desktop: hide overflow menu until row hover, keyboard focus, or open menu (touch keeps it visible). */
function sidebarEllipsisTriggerRowClass(isMobile: boolean): string {
  if (isMobile) return SIDEBAR_ELLIPSIS_TRIGGER_ROW_BASE;
  return `${SIDEBAR_ELLIPSIS_TRIGGER_ROW_BASE} transition-opacity duration-150 opacity-0 pointer-events-none group-hover:pointer-events-auto group-hover:opacity-100 group-focus-within:pointer-events-auto group-focus-within:opacity-100 has-[[data-state=open]]:pointer-events-auto has-[[data-state=open]]:opacity-100`;
}
const SIDEBAR_ELLIPSIS_BTN =
  "relative z-10 shrink-0 rounded-full border-0 bg-transparent p-1.5 text-foreground/40 transition-colors hover:text-foreground group-hover:text-foreground focus-visible:text-foreground focus-visible:outline-none";

interface ChatHistoryListProps {
  currentChatId?: string;
  searchQuery?: string;
  isMobile?: boolean;
  isSelectionMode?: boolean;
  onExitSelectionMode?: () => void;
  selectedIds: Set<string>;
  onSelectionChange: (ids: Set<string>) => void;
  containerRef?: React.RefObject<HTMLElement>;
}

export function ChatHistoryList({
  currentChatId,
  searchQuery = "",
  isMobile = false,
  isSelectionMode = false,
  onExitSelectionMode,
  selectedIds,
  onSelectionChange,
  containerRef
}: ChatHistoryListProps) {
  const opensecret = useOpenSecret();
  const openai = useOpenAI();
  const router = useRouter();
  const queryClient = useQueryClient();
  const { selectedProjectId, setSelectedProjectId } = useSelectedProjectState();
  const userId = opensecret.auth.user?.user.id;
  const runtimeStore = useChatRuntimeStore<Conversation, unknown>();
  const activeRunKeys = useSyncExternalStore(
    runtimeStore.subscribeActiveRuns,
    runtimeStore.getActiveRunKeys,
    runtimeStore.getActiveRunKeys
  );
  const completedUnreadKeys = useSyncExternalStore(
    runtimeStore.subscribeCompletedUnread,
    runtimeStore.getCompletedUnreadKeys,
    runtimeStore.getCompletedUnreadKeys
  );
  const activeRunKeySet = useMemo(() => new Set(activeRunKeys), [activeRunKeys]);
  const completedUnreadKeySet = useMemo(() => new Set(completedUnreadKeys), [completedUnreadKeys]);
  const activeRunProjectIds = useMemo(() => {
    const projectIds = new Set<string>();
    for (const key of activeRunKeys) {
      const projectId = runtimeStore.getActiveRunGroupId(key);
      if (projectId) projectIds.add(projectId);
    }
    return projectIds;
  }, [activeRunKeys, runtimeStore]);
  const completedUnreadProjectIds = useMemo(() => {
    const projectIds = new Set<string>();
    for (const key of completedUnreadKeys) {
      const projectId = runtimeStore.getCompletedUnreadGroupId(key);
      if (projectId) projectIds.add(projectId);
    }
    return projectIds;
  }, [completedUnreadKeys, runtimeStore]);
  const [isRenameDialogOpen, setIsRenameDialogOpen] = useState(false);
  const [isDeleteDialogOpen, setIsDeleteDialogOpen] = useState(false);
  const [isBulkDeleteDialogOpen, setIsBulkDeleteDialogOpen] = useState(false);
  const [isMoveDialogOpen, setIsMoveDialogOpen] = useState(false);
  const [isBulkDeleting, setIsBulkDeleting] = useState(false);
  const [isBulkMoving, setIsBulkMoving] = useState(false);
  const [isProjectDialogOpen, setIsProjectDialogOpen] = useState(false);
  const [projectDialogMode, setProjectDialogMode] = useState<"create" | "rename">("create");
  const [isDeleteProjectDialogOpen, setIsDeleteProjectDialogOpen] = useState(false);
  const [selectedChat, setSelectedChat] = useState<{ id: string; title: string } | null>(null);
  const [selectedProject, setSelectedProject] = useState<ConversationProjectListItem | null>(null);
  const [expandedProjectId, setExpandedProjectId] = useState<string | null>(selectedProjectId);
  const longPressTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const preventProjectMenuFocusRestoreRef = useRef(false);

  // Pagination states
  const [oldestConversationId, setOldestConversationId] = useState<string | undefined>();
  const [hasMoreConversations, setHasMoreConversations] = useState(false);
  const [isLoadingMore, setIsLoadingMore] = useState(false);
  const isLoadingMoreRef = useRef(false);
  const lastConversationRef = useRef<HTMLDivElement>(null);
  const [conversations, setConversations] = useState<Conversation[]>([]);

  useEffect(() => {
    setConversations([]);
    setOldestConversationId(undefined);
    setHasMoreConversations(false);
    setIsLoadingMore(false);
    isLoadingMoreRef.current = false;
  }, [userId]);

  // Pull-to-refresh (imperative updates to avoid reflow/re-render jank on iOS)
  const [isPullRefreshing, setIsPullRefreshing] = useState(false);
  const pullStartY = useRef(0);
  const isPulling = useRef(false);
  const pullDistanceRef = useRef(0);
  const isRefreshingRef = useRef(false);
  const pullRafRef = useRef<number | null>(null);
  const pullContentRef = useRef<HTMLDivElement>(null);
  const pullIndicatorRef = useRef<HTMLDivElement>(null);
  const refreshIconRef = useRef<SVGSVGElement>(null);

  const getConversationTitle = useCallback((conversation: Conversation) => {
    const rawTitle = conversation.metadata?.title;
    return typeof rawTitle === "string" && rawTitle.trim() ? rawTitle : "Untitled Chat";
  }, []);

  const invalidateConversationData = useCallback(async () => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ["conversations"] }),
      queryClient.invalidateQueries({ queryKey: ["pinnedConversations"] }),
      queryClient.invalidateQueries({ queryKey: ["projectConversations"] }),
      queryClient.invalidateQueries({ queryKey: ["conversationProjects"] }),
      queryClient.invalidateQueries({ queryKey: ["conversationProject"] })
    ]);
  }, [queryClient]);

  const applyPullStyles = useCallback(() => {
    const distance = pullDistanceRef.current;

    if (pullContentRef.current) {
      pullContentRef.current.style.transform = `translate3d(0, ${distance}px, 0)`;
    }

    if (pullIndicatorRef.current) {
      const clamped = Math.min(distance, 60);
      pullIndicatorRef.current.style.opacity = `${Math.min(distance / 60, 1)}`;
      pullIndicatorRef.current.style.transform = `translate3d(0, ${clamped - 60}px, 0)`;
    }

    if (refreshIconRef.current) {
      refreshIconRef.current.style.transform = isRefreshingRef.current
        ? "none"
        : `rotate(${distance * 3}deg)`;
    }
  }, []);

  const scheduleApplyPullStyles = useCallback(() => {
    if (pullRafRef.current != null) return;
    pullRafRef.current = requestAnimationFrame(() => {
      pullRafRef.current = null;
      applyPullStyles();
    });
  }, [applyPullStyles]);

  const setPullDistancePx = useCallback(
    (distance: number, opts?: { transition?: boolean }) => {
      pullDistanceRef.current = distance;

      if (pullContentRef.current) {
        pullContentRef.current.style.transition = opts?.transition
          ? "transform 200ms ease-out"
          : "none";
      }

      scheduleApplyPullStyles();
    },
    [scheduleApplyPullStyles]
  );

  useEffect(() => {
    setPullDistancePx(0);
    return () => {
      if (pullRafRef.current != null) {
        cancelAnimationFrame(pullRafRef.current);
        pullRafRef.current = null;
      }
    };
  }, [setPullDistancePx]);

  // Fetch initial conversations from API using the OpenSecret SDK
  const {
    data: initialConversations,
    isPending,
    error
  } = useQuery({
    queryKey: conversationHistoryQueryKey(userId),
    queryFn: async () => {
      const response = await opensecret.listConversations({
        limit: 20,
        unassigned_project: true
      });
      return response.data || [];
    },
    enabled: shouldLoadConversationHistory(userId),
    retry: INITIAL_CHAT_HISTORY_RETRY_COUNT,
    retryDelay: 250
    // No refetchInterval - we'll use manual polling instead
  });

  // Query data is authoritative across remounts and request deduplication. Keeping
  // setState outside queryFn ensures a newly mounted sidebar consumes a request
  // that may have been started by the previous authenticated surface.
  useEffect(() => {
    if (!userId || initialConversations === undefined) return;
    const page = initialChatHistoryPage(initialConversations);
    setConversations([...page.conversations]);
    setOldestConversationId(page.oldestConversationId);
    setHasMoreConversations(page.hasMoreConversations);
  }, [initialConversations, userId]);

  // Smart polling function - only fetches NEW conversations using the same pattern as UnifiedChat
  const pollForUpdates = useCallback(async () => {
    if (!userId) return;

    try {
      // Fetch latest conversations to detect new ones or metadata changes
      // Default order is desc (newest first), fetches up to 20 conversations
      const response = await opensecret.listConversations({
        limit: 20, // Fetch up to 20 to catch any new conversations
        unassigned_project: true
      });

      const newConversations = response.data || [];

      if (newConversations.length > 0) {
        // Use server ordering as the source of truth for the latest page.
        // This prevents "gap filler" items (after a delete) from being incorrectly prepended.
        setConversations((prev) => {
          const prevById = new Map(prev.map((c) => [c.id, c]));

          const addedIds = new Set<string>();
          const next: Conversation[] = [];

          for (const c of newConversations) {
            if (addedIds.has(c.id)) continue;
            addedIds.add(c.id);

            const existing = prevById.get(c.id);
            if (existing && existing.metadata?.title === c.metadata?.title) {
              next.push(existing);
              continue;
            }

            next.push(c);
          }

          // Preserve any older conversations we already loaded via pagination.
          for (const existing of prev) {
            if (addedIds.has(existing.id)) continue;
            addedIds.add(existing.id);
            next.push(existing);
          }

          if (next.length === prev.length && next.every((c, i) => c === prev[i])) {
            return prev;
          }

          return next;
        });
      }
    } catch (error) {
      console.error("Polling error:", error);
      // Fail silently - don't disrupt the UI
    }
  }, [opensecret, userId]);

  // Pull-to-refresh handler
  const handleRefresh = useCallback(async () => {
    isRefreshingRef.current = true;
    setIsPullRefreshing(true);
    setPullDistancePx(60, { transition: true });
    try {
      await pollForUpdates();
    } catch (error) {
      console.error("Refresh failed:", error);
    } finally {
      setTimeout(() => {
        setIsPullRefreshing(false);
        isRefreshingRef.current = false;
        setPullDistancePx(0, { transition: true });
      }, 300);
    }
  }, [pollForUpdates, setPullDistancePx]);

  // Pull-to-refresh event handlers - unified for all platforms (touch + mouse drag only)
  useEffect(() => {
    const container = containerRef?.current;
    if (!container) return;

    const handleTouchStart = (e: TouchEvent) => {
      if (container.scrollTop <= 0 && !isRefreshingRef.current) {
        if (longPressTimerRef.current) {
          clearTimeout(longPressTimerRef.current);
          longPressTimerRef.current = null;
        }
        pullStartY.current = e.touches[0].clientY;
        isPulling.current = true;
        setPullDistancePx(0);
      }
    };

    const handleTouchMove = (e: TouchEvent) => {
      if (!isPulling.current || isRefreshingRef.current) return;

      const currentY = e.touches[0].clientY;
      const distance = currentY - pullStartY.current;

      if (distance > 0 && container.scrollTop <= 0) {
        e.preventDefault();
        if (longPressTimerRef.current) {
          clearTimeout(longPressTimerRef.current);
          longPressTimerRef.current = null;
        }
        const resistanceFactor = 0.4;
        const adjustedDistance = Math.min(distance * resistanceFactor, 80);
        setPullDistancePx(adjustedDistance);
      }
    };

    const handleTouchEnd = () => {
      if (!isPulling.current) return;
      isPulling.current = false;

      if (pullDistanceRef.current > 60) {
        setPullDistancePx(60, { transition: true });
        handleRefresh();
      } else {
        setPullDistancePx(0, { transition: true });
      }
    };

    const handleMouseDown = (e: MouseEvent) => {
      if (e.button !== 0) return;
      if (container.scrollTop <= 0 && !isRefreshingRef.current) {
        const target = e.target as HTMLElement;
        if (target.closest('button, a, input, [role="menuitem"]')) return;
        if (longPressTimerRef.current) {
          clearTimeout(longPressTimerRef.current);
          longPressTimerRef.current = null;
        }
        pullStartY.current = e.clientY;
        isPulling.current = true;
        setPullDistancePx(0);
      }
    };

    const handleMouseMove = (e: MouseEvent) => {
      if (!isPulling.current || isRefreshingRef.current) return;

      const currentY = e.clientY;
      const distance = currentY - pullStartY.current;

      if (distance > 0 && container.scrollTop <= 0) {
        e.preventDefault();
        if (longPressTimerRef.current) {
          clearTimeout(longPressTimerRef.current);
          longPressTimerRef.current = null;
        }
        const resistanceFactor = 0.4;
        const adjustedDistance = Math.min(distance * resistanceFactor, 80);
        setPullDistancePx(adjustedDistance);
      }
    };

    const handleMouseUp = () => {
      if (!isPulling.current) return;
      isPulling.current = false;

      if (pullDistanceRef.current > 60) {
        setPullDistancePx(60, { transition: true });
        handleRefresh();
      } else {
        setPullDistancePx(0, { transition: true });
      }
    };

    // Touch events for mobile
    container.addEventListener("touchstart", handleTouchStart, { passive: true });
    container.addEventListener("touchmove", handleTouchMove, { passive: false });
    container.addEventListener("touchend", handleTouchEnd);
    container.addEventListener("touchcancel", handleTouchEnd);

    // Mouse events for desktop (click and drag)
    container.addEventListener("mousedown", handleMouseDown);
    window.addEventListener("mousemove", handleMouseMove);
    window.addEventListener("mouseup", handleMouseUp);

    return () => {
      container.removeEventListener("touchstart", handleTouchStart);
      container.removeEventListener("touchmove", handleTouchMove);
      container.removeEventListener("touchend", handleTouchEnd);
      container.removeEventListener("touchcancel", handleTouchEnd);
      container.removeEventListener("mousedown", handleMouseDown);
      window.removeEventListener("mousemove", handleMouseMove);
      window.removeEventListener("mouseup", handleMouseUp);
    };
  }, [containerRef, handleRefresh, setPullDistancePx]);

  // Set up polling every 60 seconds
  useEffect(() => {
    if (!userId) return;

    const intervalId = setInterval(() => {
      pollForUpdates();
    }, 60000); // Poll every 60 seconds

    return () => {
      clearInterval(intervalId);
    };
  }, [pollForUpdates, userId]);

  // Load more older conversations for pagination
  const loadMoreConversations = useCallback(async () => {
    if (!opensecret || !oldestConversationId || isLoadingMoreRef.current) return;

    isLoadingMoreRef.current = true;

    setIsLoadingMore(true);

    try {
      // Fetch next 10 older conversations using the oldest conversation ID we have
      const response = await opensecret.listConversations({
        limit: 10,
        after: oldestConversationId,
        unassigned_project: true
      });

      const olderConversations = response.data || [];

      if (olderConversations.length > 0) {
        // Append older conversations to the end of existing conversations
        setConversations((prev) => {
          const seen = new Set(prev.map((c) => c.id));
          const merged = [...prev];

          for (const c of olderConversations) {
            if (seen.has(c.id)) continue;
            seen.add(c.id);
            merged.push(c);
          }

          return merged;
        });

        // Update pagination state
        const newOldestId = olderConversations[olderConversations.length - 1].id;
        setOldestConversationId(newOldestId);
        setHasMoreConversations(olderConversations.length === 10);
      } else {
        // No more conversations to load
        setHasMoreConversations(false);
      }
    } catch (error) {
      console.error("Failed to load more conversations:", error);
    } finally {
      isLoadingMoreRef.current = false;
      setIsLoadingMore(false);
    }
  }, [opensecret, oldestConversationId]);

  // Set up IntersectionObserver for loading more conversations
  useEffect(() => {
    if (!lastConversationRef.current || !hasMoreConversations) return;

    const observer = new IntersectionObserver(
      (entries) => {
        // When the last conversation comes into view, load more
        if (entries[0].isIntersecting && hasMoreConversations && !isLoadingMore) {
          loadMoreConversations();
        }
      },
      {
        rootMargin: "100px", // Start loading a bit before reaching the bottom
        threshold: 0.1
      }
    );

    observer.observe(lastConversationRef.current);

    return () => {
      observer.disconnect();
    };
  }, [hasMoreConversations, isLoadingMore, loadMoreConversations]);

  const { data: conversationProjects = [] } = useQuery({
    queryKey: ["conversationProjects", userId],
    queryFn: () => listAllConversationProjects(opensecret),
    enabled: !!userId
  });

  const { data: expandedProjectConversations = [] } = useQuery({
    queryKey: ["projectConversations", userId, expandedProjectId],
    queryFn: () => {
      if (!expandedProjectId) return [];
      return listAllConversations(opensecret, {
        project_id: expandedProjectId
      });
    },
    enabled: !!userId && !!expandedProjectId
  });

  const { data: pinnedConversations = [] } = useQuery({
    queryKey: ["pinnedConversations", userId],
    queryFn: () =>
      listAllConversations(opensecret, {
        pinned: true
      }),
    enabled: !!userId
  });

  useEffect(() => {
    if (
      selectedProjectId &&
      conversationProjects.length > 0 &&
      !conversationProjects.some((project) => project.id === selectedProjectId)
    ) {
      setSelectedProjectId(null);
    }
  }, [selectedProjectId, conversationProjects, setSelectedProjectId]);

  useEffect(() => {
    if (selectedProjectId) {
      setExpandedProjectId(selectedProjectId);
    }
  }, [selectedProjectId]);

  useEffect(() => {
    if (
      expandedProjectId &&
      conversationProjects.length > 0 &&
      !conversationProjects.some((project) => project.id === expandedProjectId)
    ) {
      setExpandedProjectId(null);
    }
  }, [expandedProjectId, conversationProjects]);

  const normalizedQuery = searchQuery.trim().toLowerCase();

  const filteredProjects = useMemo(() => {
    if (!normalizedQuery) return conversationProjects;

    return conversationProjects.filter((project) => {
      if (project.name.toLowerCase().includes(normalizedQuery)) {
        return true;
      }

      if (project.id !== expandedProjectId) {
        return false;
      }

      return expandedProjectConversations.some((conversation) =>
        getConversationTitle(conversation).toLowerCase().includes(normalizedQuery)
      );
    });
  }, [
    conversationProjects,
    expandedProjectConversations,
    expandedProjectId,
    getConversationTitle,
    normalizedQuery
  ]);

  const filteredExpandedProjectConversations = useMemo(() => {
    if (!normalizedQuery) return expandedProjectConversations;

    return expandedProjectConversations.filter((conversation) =>
      getConversationTitle(conversation).toLowerCase().includes(normalizedQuery)
    );
  }, [expandedProjectConversations, getConversationTitle, normalizedQuery]);

  const filteredPinnedConversations = useMemo(() => {
    if (!normalizedQuery) return pinnedConversations;

    return pinnedConversations.filter((conversation) =>
      getConversationTitle(conversation).toLowerCase().includes(normalizedQuery)
    );
  }, [getConversationTitle, normalizedQuery, pinnedConversations]);

  const filteredRecentConversations = useMemo(() => {
    const filtered = conversations.filter(
      (conversation) => !conversation.pinned && !conversation.project_id
    );

    if (!normalizedQuery) return filtered;

    return filtered.filter((conversation) =>
      getConversationTitle(conversation).toLowerCase().includes(normalizedQuery)
    );
  }, [conversations, getConversationTitle, normalizedQuery]);

  const dispatchConversationMetadataUpdated = useCallback(
    (conversationId: string, updates: Record<string, unknown>) => {
      window.dispatchEvent(
        new CustomEvent("conversationmetadataupdated", {
          detail: { conversationId, ...updates }
        })
      );
    },
    []
  );

  const updateRuntimeConversationProject = useCallback(
    (conversationId: string, projectId: string | null) => {
      const key = createConversationChatKey(conversationId);
      if (runtimeStore.get(key)) {
        runtimeStore.update(key, (snapshot) => ({
          ...snapshot,
          conversation: snapshot.conversation
            ? { ...snapshot.conversation, project_id: projectId }
            : null
        }));
      }
      runtimeStore.updateActivityGroup(key, projectId);
    },
    [runtimeStore]
  );

  // Handle conversation deletion via API
  const handleDeleteConversation = useCallback(
    async (conversationId: string) => {
      const expectedUserId = opensecret.auth.user?.user.id;
      const releaseDeletionFence = beginChatRuntimeDeletionFence(
        runtimeStore,
        createConversationChatKey(conversationId)
      );
      try {
        assertChatAccountCredential(expectedUserId);
        await quiesceChatRuntimeRunsForHistoryDeletion({
          store: runtimeStore,
          keys: [createConversationChatKey(conversationId)],
          responseOwnershipClient: openai,
          cancelResponse: (responseId) => cancelChatResponseForHistoryDeletion(openai, responseId),
          settleCancelledRun: (key, runToken) =>
            settleChatRuntimeAfterHistoryCancellation(runtimeStore, key, runToken)
        });
        assertChatAccountCredential(expectedUserId);
        await opensecret.deleteConversation(conversationId);

        if (conversationId === currentChatId) {
          const params = new URLSearchParams(window.location.search);
          params.delete("conversation_id");
          window.history.replaceState({}, "", params.toString() ? `/?${params}` : "/");
          window.dispatchEvent(new Event("newchat"));
        }

        setConversations((prev) => prev.filter((conv) => conv.id !== conversationId));
        try {
          await invalidateConversationData();
        } finally {
          // Let the replacement draft commit before removing a runtime that
          // UnifiedChat may still be subscribed to during this event turn.
          runtimeStore.delete(createConversationChatKey(conversationId));
        }
      } catch (error) {
        console.error("Error deleting conversation:", error);
      } finally {
        releaseDeletionFence();
      }
    },
    [currentChatId, invalidateConversationData, openai, opensecret, runtimeStore]
  );

  const MAX_SELECTION = 20;

  // Toggle selection of a single chat
  const toggleSelection = useCallback(
    (chatId: string) => {
      const newSelection = new Set(selectedIds);
      if (newSelection.has(chatId)) {
        newSelection.delete(chatId);
      } else {
        // Enforce max selection limit
        if (newSelection.size >= MAX_SELECTION) {
          return;
        }
        newSelection.add(chatId);
      }
      onSelectionChange(newSelection);
    },
    [selectedIds, onSelectionChange]
  );

  // Handle bulk delete
  const handleBulkDelete = useCallback(async () => {
    if (selectedIds.size === 0) return;

    setIsBulkDeleting(true);
    const releaseDeletionFences = Array.from(selectedIds, (id) =>
      beginChatRuntimeDeletionFence(runtimeStore, createConversationChatKey(id))
    );
    try {
      const expectedUserId = opensecret.auth.user?.user.id;
      assertChatAccountCredential(expectedUserId);
      const idsToDelete = Array.from(selectedIds);
      const keysToDelete = idsToDelete.map((id) => createConversationChatKey(id));
      const deletedIds = new Set<string>();

      if (opensecret) {
        await quiesceChatRuntimeRunsForHistoryDeletion({
          store: runtimeStore,
          keys: keysToDelete,
          responseOwnershipClient: openai,
          cancelResponse: (responseId) => cancelChatResponseForHistoryDeletion(openai, responseId),
          settleCancelledRun: (key, runToken) =>
            settleChatRuntimeAfterHistoryCancellation(runtimeStore, key, runToken)
        });
        assertChatAccountCredential(expectedUserId);
        const result = await opensecret.batchDeleteConversations(idsToDelete);

        result.data.filter((item) => item.deleted).forEach((item) => deletedIds.add(item.id));
        // If current chat was deleted, project a replacement before the old
        // runtime is removed so useSyncExternalStore never observes a gap.
        if (currentChatId && deletedIds.has(currentChatId)) {
          const params = new URLSearchParams(window.location.search);
          params.delete("conversation_id");
          window.history.replaceState({}, "", params.toString() ? `/?${params}` : "/");
          window.dispatchEvent(new Event("newchat"));
        }

        setConversations((prev) => prev.filter((conv) => !deletedIds.has(conv.id)));
        try {
          await invalidateConversationData();
        } finally {
          for (const id of deletedIds) {
            runtimeStore.delete(createConversationChatKey(id));
          }
        }
      }

      // Clear selection and exit selection mode
      onSelectionChange(new Set());
      onExitSelectionMode?.();
      setIsBulkDeleteDialogOpen(false);
    } catch (error) {
      console.error("Error bulk deleting chats:", error);
    } finally {
      for (const releaseDeletionFence of releaseDeletionFences) releaseDeletionFence();
      setIsBulkDeleting(false);
    }
  }, [
    selectedIds,
    openai,
    opensecret,
    invalidateConversationData,
    currentChatId,
    onSelectionChange,
    onExitSelectionMode,
    runtimeStore
  ]);

  // Long press handlers for mobile selection mode activation
  const handleLongPressStart = useCallback(
    (chatId: string) => {
      if (isSelectionMode) return; // Already in selection mode

      longPressTimerRef.current = setTimeout(() => {
        // Enter selection mode and select this chat
        onSelectionChange(new Set([chatId]));
      }, 500); // 500ms long press
    },
    [isSelectionMode, onSelectionChange]
  );

  const handleLongPressEnd = useCallback(() => {
    if (longPressTimerRef.current) {
      clearTimeout(longPressTimerRef.current);
      longPressTimerRef.current = null;
    }
  }, []);

  // Cancel long-press when user starts scrolling (touch move)
  const handleLongPressMove = useCallback(() => {
    if (longPressTimerRef.current) {
      clearTimeout(longPressTimerRef.current);
      longPressTimerRef.current = null;
    }
  }, []);

  // Cleanup long-press timer on unmount
  useEffect(() => {
    return () => {
      if (longPressTimerRef.current) {
        clearTimeout(longPressTimerRef.current);
        longPressTimerRef.current = null;
      }
    };
  }, []);

  // Expose bulk delete dialog trigger
  useEffect(() => {
    const handleOpenBulkDelete = () => {
      if (selectedIds.size > 0) {
        setIsBulkDeleteDialogOpen(true);
      }
    };
    window.addEventListener("openbulkdelete", handleOpenBulkDelete);
    return () => window.removeEventListener("openbulkdelete", handleOpenBulkDelete);
  }, [selectedIds.size]);

  useEffect(() => {
    const handleOpenBulkMove = () => {
      if (selectedIds.size > 0) {
        setIsMoveDialogOpen(true);
      }
    };
    window.addEventListener("openbulkmove", handleOpenBulkMove);
    return () => window.removeEventListener("openbulkmove", handleOpenBulkMove);
  }, [selectedIds.size]);

  const handleMoveConversationToProject = useCallback(
    async (conversation: Conversation, projectId: string | null) => {
      try {
        await opensecret.batchUpdateConversationProject([conversation.id], projectId);
        updateRuntimeConversationProject(conversation.id, projectId);
        await invalidateConversationData();

        if (conversation.id === currentChatId) {
          setSelectedProjectId(projectId);
        }

        dispatchConversationMetadataUpdated(conversation.id, {
          projectId
        });
      } catch (error) {
        console.error("Error moving conversation to project:", error);
      }
    },
    [
      currentChatId,
      dispatchConversationMetadataUpdated,
      invalidateConversationData,
      opensecret,
      setSelectedProjectId,
      updateRuntimeConversationProject
    ]
  );

  const handleToggleConversationPin = useCallback(
    async (conversation: Conversation) => {
      try {
        await opensecret.updateConversation(conversation.id, undefined, {
          pinned: !conversation.pinned
        });
        await invalidateConversationData();
        dispatchConversationMetadataUpdated(conversation.id, {
          pinned: !conversation.pinned
        });
      } catch (error) {
        console.error("Error toggling conversation pin:", error);
      }
    },
    [dispatchConversationMetadataUpdated, invalidateConversationData, opensecret]
  );

  const handleMoveSelectedConversations = useCallback(
    async (projectId: string | null) => {
      if (selectedIds.size === 0) return;

      setIsBulkMoving(true);
      try {
        const ids = Array.from(selectedIds);
        await opensecret.batchUpdateConversationProject(ids, projectId);
        for (const id of ids) updateRuntimeConversationProject(id, projectId);
        await invalidateConversationData();

        if (currentChatId && selectedIds.has(currentChatId)) {
          setSelectedProjectId(projectId);
        }

        onSelectionChange(new Set());
        onExitSelectionMode?.();
      } catch (error) {
        console.error("Error moving selected chats:", error);
        throw error;
      } finally {
        setIsBulkMoving(false);
      }
    },
    [
      currentChatId,
      invalidateConversationData,
      onExitSelectionMode,
      onSelectionChange,
      opensecret,
      selectedIds,
      setSelectedProjectId,
      updateRuntimeConversationProject
    ]
  );

  const handleToggleProjectExpanded = useCallback((projectId: string) => {
    setExpandedProjectId((currentProjectId) => (currentProjectId === projectId ? null : projectId));
  }, []);

  const handleViewProject = useCallback(
    async (projectId: string) => {
      setSelectedProjectId(projectId);
      setExpandedProjectId(projectId);

      if (window.location.pathname !== "/") {
        await router.navigate({ to: "/" });
      }

      const params = new URLSearchParams(window.location.search);
      params.delete("conversation_id");
      params.set("project_id", projectId);

      window.history.replaceState({}, "", params.toString() ? `/?${params.toString()}` : "/");
      window.dispatchEvent(new Event("projectselected"));
    },
    [router, setSelectedProjectId]
  );

  const handleNewChatInProject = useCallback(
    async (projectId: string) => {
      preventProjectMenuFocusRestoreRef.current = true;
      setSelectedProjectId(projectId);

      if (window.location.pathname !== "/") {
        await router.navigate({ to: "/" });
      }

      const params = new URLSearchParams(window.location.search);
      params.delete("conversation_id");
      params.delete("project_id");
      const url = params.toString() ? `/?${params.toString()}` : "/";
      const draftRuntimeKey = resumeOrCreateChatDraftKey(runtimeStore, projectId);
      const detail = pushChatHistoryEntryForDraft(window.history, url, projectId, draftRuntimeKey);
      window.dispatchEvent(new CustomEvent<NewChatNavigationDetail>("newchat", { detail }));
      setTimeout(() => document.getElementById("message")?.focus(), 0);
    },
    [router, runtimeStore, setSelectedProjectId]
  );

  const handleCreateProject = useCallback(
    async (name: string) => {
      const project = await opensecret.createConversationProject({ name });
      await invalidateConversationData();
      await handleViewProject(project.id);
    },
    [handleViewProject, invalidateConversationData, opensecret]
  );

  const handleRenameProject = useCallback(
    async (name: string) => {
      if (!selectedProject) return;
      await opensecret.updateConversationProject(selectedProject.id, { name });
      await invalidateConversationData();
    },
    [invalidateConversationData, opensecret, selectedProject]
  );

  const handleDeleteProject = useCallback(async () => {
    if (!selectedProject) return;
    let replacementDispatched = false;
    const projectConversationKeys = new Set<ReturnType<typeof createConversationChatKey>>();
    const projectAwayFromDeletedProject = () => {
      if (replacementDispatched) return;
      const deletingProjectedChat =
        selectedProjectId === selectedProject.id ||
        (currentChatId
          ? projectConversationKeys.has(createConversationChatKey(currentChatId)) ||
            runtimeStore.getActivityGroupId(createConversationChatKey(currentChatId)) ===
              selectedProject.id
          : false);
      if (!deletingProjectedChat) return;

      replacementDispatched = true;
      setSelectedProjectId(null);
      const params = new URLSearchParams(window.location.search);
      params.delete("conversation_id");
      params.delete("project_id");
      const draftRuntimeKey = rootChatDraftKeyAfterProjectDeletion(runtimeStore);
      const chatEntry = createChatHistoryEntryForDraft(draftRuntimeKey);
      window.history.replaceState(
        chatEntry.historyState,
        "",
        params.toString() ? `/?${params.toString()}` : "/"
      );
      window.dispatchEvent(
        new CustomEvent<NewChatNavigationDetail>("newchat", {
          detail: { projectId: null, draftRuntimeKey: chatEntry.draftRuntimeKey }
        })
      );
      window.dispatchEvent(new Event("projectselected"));
    };

    // Until server membership is known, conservatively pause every local
    // runner. Replace this short discovery fence synchronously with exact-key
    // plus activity-group fences before the destructive request starts.
    const releaseDiscoveryFence = beginAllChatRuntimeDeletionFence(runtimeStore);
    let releaseDeletionFence: (() => void) | null = null;
    try {
      const expectedUserId = opensecret.auth.user?.user.id;
      assertChatAccountCredential(expectedUserId);
      const projectConversations = await listAllConversations(opensecret, {
        project_id: selectedProject.id
      });
      for (const conversation of projectConversations) {
        projectConversationKeys.add(createConversationChatKey(conversation.id));
      }
      releaseDeletionFence = beginChatProjectRuntimeDeletionFence(
        runtimeStore,
        selectedProject.id,
        Array.from(projectConversationKeys)
      );
      releaseDiscoveryFence();

      await quiesceChatRuntimeRunsForHistoryDeletion({
        store: runtimeStore,
        keys: Array.from(projectConversationKeys),
        activityGroupId: selectedProject.id,
        responseOwnershipClient: openai,
        cancelResponse: (responseId) => cancelChatResponseForHistoryDeletion(openai, responseId),
        settleCancelledRun: (key, runToken) =>
          settleChatRuntimeAfterHistoryCancellation(runtimeStore, key, runToken)
      });

      assertChatAccountCredential(expectedUserId);
      await opensecret.deleteConversationProject(selectedProject.id);
      projectAwayFromDeletedProject();

      if (expandedProjectId === selectedProject.id) {
        setExpandedProjectId(null);
      }

      try {
        await invalidateConversationData();
      } finally {
        // Metadata can finish loading while the delete request/refetch is in
        // flight, so re-check membership immediately before grouped cleanup.
        projectAwayFromDeletedProject();
        // A selected chat in this project has now committed its replacement;
        // grouped background runtimes can be aborted and discarded safely.
        for (const key of projectConversationKeys) runtimeStore.delete(key);
        runtimeStore.deleteActivityGroup(selectedProject.id);
      }
    } catch (error) {
      console.error("Error deleting project:", error);
      throw error;
    } finally {
      releaseDeletionFence?.();
      releaseDiscoveryFence();
    }
  }, [
    currentChatId,
    expandedProjectId,
    invalidateConversationData,
    openai,
    opensecret,
    selectedProject,
    selectedProjectId,
    runtimeStore,
    setSelectedProjectId
  ]);

  const handleOpenRenameDialog = useCallback(
    (conv: Conversation) => {
      setSelectedChat({ id: conv.id, title: getConversationTitle(conv) });
      setIsRenameDialogOpen(true);
    },
    [getConversationTitle]
  );

  const handleOpenDeleteDialog = useCallback(
    (conv: Conversation) => {
      setSelectedChat({ id: conv.id, title: getConversationTitle(conv) });
      setIsDeleteDialogOpen(true);
    },
    [getConversationTitle]
  );

  const handleOpenCreateProjectDialog = useCallback(() => {
    setSelectedProject(null);
    setProjectDialogMode("create");
    setIsProjectDialogOpen(true);
  }, []);

  const handleOpenRenameProjectDialog = useCallback((project: ConversationProjectListItem) => {
    setSelectedProject(project);
    setProjectDialogMode("rename");
    setIsProjectDialogOpen(true);
  }, []);

  const handleOpenDeleteProjectDialog = useCallback((project: ConversationProjectListItem) => {
    setSelectedProject(project);
    setIsDeleteProjectDialogOpen(true);
  }, []);

  // Handle conversation renaming via API
  const handleRenameConversation = useCallback(
    async (conversationId: string, newTitle: string) => {
      try {
        await opensecret.updateConversation(conversationId, { title: newTitle });
        setConversations((prev) =>
          prev.map((conv) =>
            conv.id === conversationId
              ? { ...conv, metadata: { ...(conv.metadata ?? {}), title: newTitle } }
              : conv
          )
        );
        await invalidateConversationData();
        dispatchConversationMetadataUpdated(conversationId, {
          metadata: { title: newTitle }
        });
      } catch (error) {
        console.error("Error renaming conversation:", error);
        throw error;
      }
    },
    [dispatchConversationMetadataUpdated, invalidateConversationData, opensecret]
  );

  // Handle conversation selection
  const handleSelectConversation = useCallback(
    async (conversation: Conversation) => {
      runtimeStore.markCompletionRead(createConversationChatKey(conversation.id));
      setSelectedProjectId(conversation.project_id ?? null);

      if (window.location.pathname !== "/") {
        await router.navigate({ to: "/" });
      }

      const params = new URLSearchParams(window.location.search);
      params.delete("project_id");
      params.set("conversation_id", conversation.id);
      window.history.pushState({}, "", `/?${params.toString()}`);

      window.dispatchEvent(
        new CustomEvent("conversationselected", {
          detail: { conversationId: conversation.id }
        })
      );
    },
    [router, runtimeStore, setSelectedProjectId]
  );

  // Listen for conversation created event to refresh the list
  useEffect(() => {
    const handleConversationCreated = () => {
      pollForUpdates();
      queryClient.invalidateQueries({ queryKey: ["projectConversations"] });
      queryClient.invalidateQueries({ queryKey: ["pinnedConversations"] });
    };

    window.addEventListener("conversationcreated", handleConversationCreated);
    return () => window.removeEventListener("conversationcreated", handleConversationCreated);
  }, [pollForUpdates, queryClient]);

  if (error && conversations.length === 0) {
    return <div>{error.message}</div>;
  }

  if (isPending) {
    return <div>Loading chat history...</div>;
  }

  const trimmedQuery = searchQuery.trim();
  if (
    trimmedQuery &&
    filteredProjects.length === 0 &&
    filteredExpandedProjectConversations.length === 0 &&
    filteredPinnedConversations.length === 0 &&
    filteredRecentConversations.length === 0
  ) {
    return (
      <div className="text-muted-foreground text-center py-4">
        <p>No chats found matching "{trimmedQuery}"</p>
        <p className="text-sm mt-1">Try a different search term</p>
      </div>
    );
  }

  const renderConversationRow = (conversation: Conversation) => {
    const title = getConversationTitle(conversation);
    const isActive = conversation.id === currentChatId;
    const isSelected = selectedIds.has(conversation.id);
    const runtimeKey = createConversationChatKey(conversation.id);
    const isRunning = activeRunKeySet.has(runtimeKey);
    const isUnreadCompleted = !isRunning && completedUnreadKeySet.has(runtimeKey);
    const titlePaddingClass = isMobile ? "pr-8" : "";

    const isBoldState = (isActive && !isSelectionMode) || (isSelectionMode && isSelected);
    const rowTextClass = isBoldState
      ? "font-bold text-foreground"
      : "text-foreground/95 group-hover:text-foreground";

    return (
      <div
        key={conversation.id}
        className="group relative isolate flex w-full min-w-0 select-none items-stretch gap-0.5 rounded-2xl"
        onContextMenu={(event) => event.preventDefault()}
      >
        <button
          type="button"
          data-testid="conversation-row"
          data-conversation-id={conversation.id}
          data-running={isRunning || undefined}
          data-unread={isUnreadCompleted || undefined}
          aria-current={isActive ? "page" : undefined}
          aria-label={
            isRunning
              ? `${title}, running`
              : isUnreadCompleted
                ? `${title}, completed, unread`
                : title
          }
          onClick={() => {
            if (isSelectionMode) {
              toggleSelection(conversation.id);
            } else {
              void handleSelectConversation(conversation);
            }
          }}
          onMouseDown={() => isMobile && handleLongPressStart(conversation.id)}
          onMouseUp={handleLongPressEnd}
          onMouseLeave={handleLongPressEnd}
          onTouchStart={() => isMobile && handleLongPressStart(conversation.id)}
          onTouchMove={handleLongPressMove}
          onTouchEnd={handleLongPressEnd}
          onTouchCancel={handleLongPressEnd}
          className={`relative ${ROW_CONTENT_Z} min-w-0 flex-1 py-1 pr-2 text-left ${rowTextClass} ${
            isSelectionMode ? "pl-8" : "pl-0"
          } cursor-pointer`}
        >
          <div className={titlePaddingClass}>
            <div className="relative z-0 flex min-w-0 items-center gap-1.5">
              {isRunning ? (
                <>
                  <Loader2
                    className="h-3 w-3 shrink-0 animate-spin text-[hsl(var(--maple-primary))]"
                    aria-hidden="true"
                  />
                  <span className="sr-only">Response in progress</span>
                </>
              ) : isUnreadCompleted ? (
                <span
                  className="h-2 w-2 shrink-0 rounded-full bg-maple-success"
                  aria-hidden="true"
                />
              ) : null}
              {conversation.pinned ? (
                <Pin
                  className="h-3.5 w-3.5 shrink-0 fill-none text-foreground"
                  strokeWidth={ICON_STROKE}
                  aria-hidden
                />
              ) : null}
              <div className={SIDEBAR_TITLE_CLASS}>{title}</div>
            </div>
          </div>
        </button>
        {isSelectionMode ? (
          <div
            className={`absolute left-1.5 top-1/2 ${ROW_CHECKBOX_Z} -translate-y-1/2`}
            onClick={(e) => e.stopPropagation()}
          >
            <Checkbox
              checked={isSelected}
              onCheckedChange={() => toggleSelection(conversation.id)}
              onClick={(event) => event.stopPropagation()}
              className="data-[state=checked]:bg-primary"
            />
          </div>
        ) : null}
        {!isSelectionMode ? (
          <>
            <div className={sidebarEllipsisTriggerRowClass(isMobile)}>
              <div className={SIDEBAR_ELLIPSIS_FADE} aria-hidden="true" />
              <div className="flex items-center bg-muted dark:bg-[hsl(var(--sidebar))]">
                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <button
                      type="button"
                      aria-label={`Open chat menu for ${title}`}
                      className={SIDEBAR_ELLIPSIS_BTN}
                      onClick={(event) => {
                        event.preventDefault();
                        event.stopPropagation();
                      }}
                    >
                      <MoreHorizontal className="h-4 w-4" strokeWidth={ICON_STROKE} />
                    </button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent>
                    <DropdownMenuItem onClick={() => onSelectionChange(new Set([conversation.id]))}>
                      <CheckSquare className="mr-2 h-4 w-4" strokeWidth={ICON_STROKE} />
                      Select
                    </DropdownMenuItem>
                    <DropdownMenuItem onClick={() => handleToggleConversationPin(conversation)}>
                      {conversation.pinned ? (
                        <PinOff className="mr-2 h-4 w-4" strokeWidth={ICON_STROKE} />
                      ) : (
                        <Pin className="mr-2 h-4 w-4" strokeWidth={ICON_STROKE} />
                      )}
                      {conversation.pinned ? "Unpin Chat" : "Pin Chat"}
                    </DropdownMenuItem>
                    <DropdownMenuSub>
                      <DropdownMenuSubTrigger>
                        <FolderInput className="mr-2 h-4 w-4" strokeWidth={ICON_STROKE} />
                        Move to Project
                      </DropdownMenuSubTrigger>
                      <DropdownMenuSubContent>
                        <DropdownMenuItem
                          onClick={() => handleMoveConversationToProject(conversation, null)}
                        >
                          <Folder className="mr-2 h-4 w-4" strokeWidth={ICON_STROKE} />
                          No project
                        </DropdownMenuItem>
                        <DropdownMenuSeparator />
                        {conversationProjects.map((project) => (
                          <DropdownMenuItem
                            key={project.id}
                            onClick={() =>
                              handleMoveConversationToProject(conversation, project.id)
                            }
                          >
                            <Folder className="mr-2 h-4 w-4" strokeWidth={ICON_STROKE} />
                            {project.name}
                          </DropdownMenuItem>
                        ))}
                      </DropdownMenuSubContent>
                    </DropdownMenuSub>
                    <DropdownMenuItem onClick={() => handleOpenRenameDialog(conversation)}>
                      <Pencil className="mr-2 h-4 w-4" strokeWidth={ICON_STROKE} />
                      Rename Chat
                    </DropdownMenuItem>
                    <DropdownMenuItem onClick={() => handleOpenDeleteDialog(conversation)}>
                      <Trash className="mr-2 h-4 w-4" strokeWidth={ICON_STROKE} />
                      Delete Chat
                    </DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              </div>
            </div>
          </>
        ) : null}
      </div>
    );
  };

  return (
    <>
      <div
        ref={pullIndicatorRef}
        className="pointer-events-none absolute left-0 right-0 top-0 z-10 flex h-[60px] items-center justify-center opacity-0"
        style={{
          transform: "translate3d(0, -60px, 0)",
          willChange: "transform, opacity"
        }}
        aria-hidden="true"
      >
        <RefreshCw
          ref={refreshIconRef}
          className={`h-4 w-4 text-muted-foreground ${isPullRefreshing ? "animate-spin" : ""}`}
          strokeWidth={ICON_STROKE}
        />
      </div>

      <div
        ref={pullContentRef}
        className="flex min-w-0 w-full max-w-full flex-col gap-5"
        style={{ willChange: "transform" }}
      >
        <div className="space-y-2">
          <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
            Projects
          </div>
          {conversationProjects.length >= MAX_PROJECTS ? (
            <Tooltip>
              <TooltipTrigger asChild>
                <button
                  type="button"
                  disabled
                  className="flex w-full items-center gap-2 rounded-2xl py-1.5 pl-0 pr-1 text-left text-muted-foreground/50 cursor-not-allowed"
                >
                  <FolderPlus className="h-4 w-4" strokeWidth={ICON_STROKE} />
                  <span>New project</span>
                </button>
              </TooltipTrigger>
              <TooltipContent side="bottom" className="bg-muted">
                <p>Maximum of {MAX_PROJECTS} projects reached</p>
              </TooltipContent>
            </Tooltip>
          ) : (
            <button
              type="button"
              onClick={handleOpenCreateProjectDialog}
              className="flex w-full items-center gap-2 rounded-2xl py-1.5 pl-0 pr-1 text-left text-foreground/95 transition-colors hover:text-foreground"
            >
              <FolderPlus className="h-4 w-4" strokeWidth={ICON_STROKE} />
              <span>New project</span>
            </button>
          )}

          {filteredProjects.map((project) => {
            const isProjectExpanded = expandedProjectId === project.id;
            const isProjectSelected = selectedProjectId === project.id;
            const showProjectRunningIndicator =
              !isProjectExpanded && activeRunProjectIds.has(project.id);
            const showProjectUnreadIndicator =
              !isProjectExpanded &&
              !showProjectRunningIndicator &&
              completedUnreadProjectIds.has(project.id);
            const projectAccessibleStatus = showProjectRunningIndicator
              ? "running"
              : showProjectUnreadIndicator
                ? "completed, unread"
                : null;
            return (
              <div
                key={project.id}
                data-testid="project-row"
                data-project-id={project.id}
                data-running={showProjectRunningIndicator || undefined}
                data-unread={showProjectUnreadIndicator || undefined}
                className="relative"
              >
                <div className="relative isolate group w-full min-w-0">
                  <button
                    type="button"
                    onClick={() => handleToggleProjectExpanded(project.id)}
                    aria-expanded={isProjectExpanded}
                    aria-label={
                      projectAccessibleStatus
                        ? `${project.name}, ${projectAccessibleStatus}`
                        : project.name
                    }
                    className={`relative ${ROW_CONTENT_Z} w-full rounded-2xl py-1 text-left ${
                      isProjectExpanded || isProjectSelected
                        ? "font-bold text-foreground"
                        : "text-foreground"
                    }`}
                  >
                    <div
                      className={`relative z-0 flex items-center gap-2 ${isMobile ? "pr-8" : ""}`}
                    >
                      {isProjectExpanded ? (
                        <ChevronDown className="h-4 w-4 shrink-0" strokeWidth={ICON_STROKE} />
                      ) : (
                        <ChevronRight className="h-4 w-4 shrink-0" strokeWidth={ICON_STROKE} />
                      )}
                      <span className={SIDEBAR_TITLE_CLASS}>{project.name}</span>
                      {showProjectRunningIndicator ? (
                        <Loader2
                          className="h-3.5 w-3.5 shrink-0 animate-spin text-[hsl(var(--maple-primary))]"
                          aria-hidden="true"
                        />
                      ) : showProjectUnreadIndicator ? (
                        <span
                          className="h-2 w-2 shrink-0 rounded-full bg-maple-success"
                          aria-hidden="true"
                        />
                      ) : null}
                    </div>
                  </button>
                  <div className={sidebarEllipsisTriggerRowClass(isMobile)}>
                    <div className={SIDEBAR_ELLIPSIS_FADE} aria-hidden="true" />
                    <div className="flex items-center bg-muted dark:bg-[hsl(var(--sidebar))]">
                      <DropdownMenu>
                        <DropdownMenuTrigger asChild>
                          <button
                            type="button"
                            aria-label={`Open project menu for ${project.name}`}
                            className={SIDEBAR_ELLIPSIS_BTN}
                            onClick={(event) => {
                              event.preventDefault();
                              event.stopPropagation();
                            }}
                          >
                            <MoreHorizontal className="h-4 w-4" strokeWidth={ICON_STROKE} />
                          </button>
                        </DropdownMenuTrigger>
                        <DropdownMenuContent
                          onCloseAutoFocus={(event) => {
                            if (!preventProjectMenuFocusRestoreRef.current) return;
                            preventProjectMenuFocusRestoreRef.current = false;
                            event.preventDefault();
                          }}
                        >
                          <DropdownMenuItem onClick={() => void handleNewChatInProject(project.id)}>
                            <SquarePen className="mr-2 h-4 w-4" strokeWidth={ICON_STROKE} />
                            New Chat in Project
                          </DropdownMenuItem>
                          <DropdownMenuItem onClick={() => void handleViewProject(project.id)}>
                            <Folder className="mr-2 h-4 w-4" strokeWidth={ICON_STROKE} />
                            View Project
                          </DropdownMenuItem>
                          <DropdownMenuSeparator />
                          <DropdownMenuItem onClick={() => handleOpenRenameProjectDialog(project)}>
                            <Pencil className="mr-2 h-4 w-4" strokeWidth={ICON_STROKE} />
                            Rename Project
                          </DropdownMenuItem>
                          <DropdownMenuItem onClick={() => handleOpenDeleteProjectDialog(project)}>
                            <Trash className="mr-2 h-4 w-4" strokeWidth={ICON_STROKE} />
                            Delete Project
                          </DropdownMenuItem>
                        </DropdownMenuContent>
                      </DropdownMenu>
                    </div>
                  </div>
                </div>

                {isProjectExpanded && filteredExpandedProjectConversations.length > 0 ? (
                  <div className="mt-2 w-full space-y-2 pl-6">
                    {filteredExpandedProjectConversations.map((conversation) =>
                      renderConversationRow(conversation)
                    )}
                  </div>
                ) : null}
              </div>
            );
          })}
        </div>

        {filteredPinnedConversations.length > 0 ? (
          <div className="space-y-2">
            <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
              Pinned
            </div>
            {filteredPinnedConversations.map((conversation) => renderConversationRow(conversation))}
          </div>
        ) : null}

        <div className="space-y-2">
          <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
            Recents
          </div>
          {filteredRecentConversations.length > 0 ? (
            filteredRecentConversations.map((conversation) => renderConversationRow(conversation))
          ) : (
            <div className="py-2 text-sm text-muted-foreground">No recent chats.</div>
          )}
        </div>

        {isLoadingMore && (
          <div className="flex items-center justify-center py-4">
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <div className="w-2 h-2 bg-foreground/60 rounded-full animate-pulse" />
              <div className="w-2 h-2 bg-foreground/60 rounded-full animate-pulse delay-75" />
              <div className="w-2 h-2 bg-foreground/60 rounded-full animate-pulse delay-150" />
            </div>
          </div>
        )}

        {!trimmedQuery && hasMoreConversations ? (
          <div ref={lastConversationRef} className="h-px w-full" aria-hidden="true" />
        ) : null}
      </div>

      {selectedChat && (
        <>
          <RenameChatDialog
            open={isRenameDialogOpen}
            onOpenChange={setIsRenameDialogOpen}
            chatId={selectedChat.id}
            currentTitle={selectedChat.title}
            onRename={handleRenameConversation}
          />
          <DeleteChatDialog
            open={isDeleteDialogOpen}
            onOpenChange={setIsDeleteDialogOpen}
            chatTitle={selectedChat.title}
            onConfirm={() => handleDeleteConversation(selectedChat.id)}
          />
        </>
      )}

      <ConversationProjectDialog
        open={isProjectDialogOpen}
        onOpenChange={setIsProjectDialogOpen}
        mode={projectDialogMode}
        initialName={selectedProject?.name}
        onSubmit={projectDialogMode === "create" ? handleCreateProject : handleRenameProject}
      />

      <DeleteConversationProjectDialog
        open={isDeleteProjectDialogOpen}
        onOpenChange={setIsDeleteProjectDialogOpen}
        projectName={selectedProject?.name ?? "Project"}
        onConfirm={handleDeleteProject}
      />

      <BulkDeleteDialog
        open={isBulkDeleteDialogOpen}
        onOpenChange={setIsBulkDeleteDialogOpen}
        onConfirm={handleBulkDelete}
        count={selectedIds.size}
        isDeleting={isBulkDeleting}
      />

      <MoveChatsDialog
        open={isMoveDialogOpen}
        onOpenChange={setIsMoveDialogOpen}
        count={selectedIds.size}
        projects={conversationProjects}
        onConfirm={handleMoveSelectedConversations}
        isMoving={isBulkMoving}
      />
    </>
  );
}
