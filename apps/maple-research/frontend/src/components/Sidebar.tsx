import {
  Search,
  SquarePenIcon,
  PanelLeft,
  Menu,
  XCircle,
  Trash2,
  X,
  FolderInput
} from "lucide-react";
import { Button } from "./ui/button";
import { useLocation, useRouter } from "@tanstack/react-router";
import { ChatHistoryList } from "./ChatHistoryList";
import { AccountMenu } from "./AccountMenu";
import { useRef, useEffect, KeyboardEvent, useCallback, useLayoutEffect, useState } from "react";
import { flushSync } from "react-dom";
import { cn, useClickOutside, useIsMobile, useIsLandscapeMobile } from "@/utils/utils";
import { MapleWordmark } from "@/components/MapleWordmark";
import { Input } from "./ui/input";
import { useSelectedProjectState, useSidebarSearchState } from "@/state/useLocalState";
import { SIDEBAR_MAX_WIDTH_CLASS, SIDEBAR_WIDTH_CLASS } from "@/constants/layout";
import { isTauriDesktop } from "@/utils/platform";
import { useOpenSecret } from "@mapleai/sdk";
import { FEATURE_FLAGS, flagsClient } from "@/services/flags";
import { rememberWorkspaceMode } from "@/services/workspaceModePreference";
import { WorkspaceModeSwitch, type WorkspaceMode } from "@/components/WorkspaceModeSwitch";
import { usePersistentHomeNavigation } from "@/contexts/PersistentHomeNavigationContext";
import { useChatRuntimeStore } from "@/contexts/ChatRuntimeContext";
import { resumeOrCreateChatDraftKey } from "@/services/chatDraftSelection";
import {
  createChatHistoryEntryForDraft,
  type NewChatNavigationDetail
} from "@/services/chatRuntimeNavigation";

export function Sidebar({
  chatId,
  isOpen,
  mode = "chat",
  onToggle
}: {
  chatId?: string;
  isOpen: boolean;
  mode?: WorkspaceMode;
  onToggle: () => void;
}) {
  const router = useRouter();
  const location = useLocation();
  const { returnToHome } = usePersistentHomeNavigation();
  const os = useOpenSecret();
  const runtimeStore = useChatRuntimeStore<unknown, unknown>();
  const userId = os.auth.user?.user.id;
  const { searchQuery, setSearchQuery, isSearchVisible, setIsSearchVisible } =
    useSidebarSearchState();
  const { setSelectedProjectId } = useSelectedProjectState();
  const searchInputRef = useRef<HTMLInputElement>(null);

  // Multi-select state
  const [isSelectionMode, setIsSelectionMode] = useState(false);
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [pendingWorkspaceMode, setPendingWorkspaceMode] = useState<WorkspaceMode | null>(null);
  const workspaceModeNavigationStartedRef = useRef(false);

  // Enter selection mode when items are selected (e.g., via long press)
  useEffect(() => {
    if (selectedIds.size > 0 && !isSelectionMode) {
      setIsSelectionMode(true);
    }
  }, [selectedIds.size, isSelectionMode]);

  const exitSelectionMode = useCallback(() => {
    setIsSelectionMode(false);
    setSelectedIds(new Set());
  }, []);

  const handleDeleteSelected = useCallback(() => {
    if (selectedIds.size > 0) {
      window.dispatchEvent(new Event("openbulkdelete"));
    }
  }, [selectedIds.size]);

  const handleMoveSelected = useCallback(() => {
    if (selectedIds.size > 0) {
      window.dispatchEvent(new Event("openbulkmove"));
    }
  }, [selectedIds.size]);

  async function addChat() {
    // If sidebar is open on compact layout, close it
    if (isOpen && isCompactLayout) {
      onToggle();
    }

    flushSync(() => {
      setSelectedProjectId(null);
    });

    if (location.pathname === "/") {
      const draftRuntimeKey = resumeOrCreateChatDraftKey(runtimeStore, null);
      const chatEntry = createChatHistoryEntryForDraft(draftRuntimeKey);
      window.history.pushState(chatEntry.historyState, "", "/");
      window.dispatchEvent(
        new CustomEvent<NewChatNavigationDetail>("newchat", {
          detail: { projectId: null, draftRuntimeKey: chatEntry.draftRuntimeKey }
        })
      );
      setTimeout(() => document.getElementById("message")?.focus(), 0);
    } else {
      try {
        // Navigate to home without any query params
        await router.navigate({ to: `/` });
        // Ensure element is available after navigation
        setTimeout(() => document.getElementById("message")?.focus(), 0);
      } catch (error) {
        console.error("Navigation failed:", error);
      }
    }
  }

  async function completeWorkspaceModeChange(nextMode: WorkspaceMode) {
    if (workspaceModeNavigationStartedRef.current) return;
    workspaceModeNavigationStartedRef.current = true;

    if (nextMode === "chat") {
      returnToHome({ replace: false });
      rememberWorkspaceMode(nextMode);
      return;
    }

    try {
      await router.navigate({ to: "/agent" });
      rememberWorkspaceMode(nextMode);
    } catch (error) {
      workspaceModeNavigationStartedRef.current = false;
      setPendingWorkspaceMode(null);
      console.error("Navigation failed:", error);
    }
  }

  function switchWorkspaceMode(nextMode: WorkspaceMode) {
    if (pendingWorkspaceMode !== null) {
      if (nextMode === mode) setPendingWorkspaceMode(null);
      return;
    }

    if (nextMode === mode) return;

    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      void completeWorkspaceModeChange(nextMode);
      return;
    }

    setPendingWorkspaceMode(nextMode);
  }

  const toggleSearch = () => {
    setIsSearchVisible(!isSearchVisible);
    if (!isSearchVisible) {
      // Focus the search input when it becomes visible
      setTimeout(() => searchInputRef.current?.focus(), 0);
    } else {
      // Clear search when hiding
      setSearchQuery("");
    }
  };

  const clearSearch = () => {
    setSearchQuery("");
    searchInputRef.current?.focus();
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Escape") {
      clearSearch();
    }
  };

  const sidebarRef = useRef<HTMLDivElement>(null);
  const historyContainerRef = useRef<HTMLElement>(null);

  // Use the centralized hooks for mobile/compact detection
  const isMobile = useIsMobile();
  const isLandscapeMobile = useIsLandscapeMobile();
  const isCompactLayout = isMobile || isLandscapeMobile;
  const agentModeAvailable = isTauriDesktop();
  const [agentModeFlag, setAgentModeFlag] = useState<{
    userId: string;
    enabled: boolean;
  } | null>(null);
  // Chat and Agent mount separate Sidebar instances. Seed a remount from the existing,
  // user-scoped flag cache while this instance revalidates in the background.
  const cachedAgentModeEnabled = userId
    ? flagsClient.peekIsEnabled(userId, FEATURE_FLAGS.AGENT_MODE)
    : undefined;
  const agentModeEnabled =
    agentModeFlag !== null && agentModeFlag.userId === userId
      ? agentModeFlag.enabled
      : cachedAgentModeEnabled;
  const showAgentMode = agentModeAvailable && agentModeEnabled === true;
  const isAgentMode = mode === "agent";

  useEffect(() => {
    if (!agentModeAvailable || !userId) return;

    let disposed = false;
    void flagsClient.isEnabled(userId, FEATURE_FLAGS.AGENT_MODE).then(
      (enabled) => {
        if (!disposed) setAgentModeFlag({ userId, enabled });
      },
      (error: unknown) => {
        console.warn("Unable to load optional feature flags; keeping them hidden.", error);
      }
    );
    return () => {
      disposed = true;
    };
  }, [agentModeAvailable, userId]);

  // Modified click outside handler to ignore clicks in dropdowns and dialogs
  // Only applies on mobile - desktop users use the toggle button
  const handleClickOutside = useCallback(
    (event: MouseEvent | TouchEvent) => {
      if (isOpen && isCompactLayout) {
        // Check if the click was inside a dropdown or dialog
        const target = event.target as HTMLElement;
        const isInDropdown = target.closest('[role="menu"]');
        const isInDialog = target.closest('[role="dialog"]');
        const isInAlertDialog = target.closest('[role="alertdialog"]');

        if (!isInDropdown && !isInDialog && !isInAlertDialog) {
          onToggle();
        }
      }
    },
    [isOpen, onToggle, isCompactLayout]
  );

  useClickOutside(sidebarRef, handleClickOutside);

  // Track if component is mounted to prevent state updates after unmount
  const isMountedRef = useRef(true);
  useLayoutEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
    };
  }, []);

  // This effect closes the sidebar on mobile when navigating,
  // but preserves search state between navigations
  useEffect(() => {
    // Only subscribe if we're on compact layout and sidebar is open
    if (!isCompactLayout || !isOpen) return;

    const unsubscribe = router.subscribe("onResolved", () => {
      // Use a microtask to avoid state updates during render
      queueMicrotask(() => {
        // Prevent updates if component unmounted
        if (!isMountedRef.current) return;

        const resolvedPath = window.location.pathname;
        const isWorkspaceModeTransition =
          (isAgentMode && resolvedPath === "/") || (!isAgentMode && resolvedPath === "/agent");
        if (isWorkspaceModeTransition) return;

        // Double-check conditions after async boundary
        if (isOpen && isCompactLayout) {
          onToggle();
        }
      });
    });

    return () => {
      unsubscribe();
    };
  }, [router, isOpen, onToggle, isCompactLayout, isAgentMode]);

  return (
    <div
      ref={sidebarRef}
      className={cn([
        "fixed md:static landscape-short:fixed z-10 h-full overflow-x-hidden overflow-y-hidden",
        isOpen ? `block ${SIDEBAR_WIDTH_CLASS}` : "hidden"
      ])}
    >
      <div
        className={cn(
          "flex h-full min-h-0 min-w-0 flex-col items-stretch overflow-x-hidden border-r border-border/20 bg-muted dark:bg-[hsl(var(--sidebar))]",
          SIDEBAR_WIDTH_CLASS,
          SIDEBAR_MAX_WIDTH_CLASS
        )}
      >
        {/* Header section */}
        <div className="flex flex-col gap-2 pt-3 pb-2">
          <div className="flex items-center pl-4 pr-[8px]">
            <div className="min-w-0 flex-1">
              <MapleWordmark className="h-4 w-auto" />
            </div>
            <button
              type="button"
              className="flex h-9 w-9 shrink-0 items-center justify-center text-foreground transition-colors hover:text-foreground/70"
              onClick={onToggle}
              aria-label="Close sidebar"
            >
              <PanelLeft className="h-4 w-4" />
            </button>
          </div>
          {(showAgentMode || isAgentMode) && (
            <div className="mb-2 px-8">
              <WorkspaceModeSwitch
                mode={pendingWorkspaceMode ?? mode}
                onModeChange={switchWorkspaceMode}
                onModeTransitionEnd={(nextMode) => {
                  if (nextMode === pendingWorkspaceMode) {
                    void completeWorkspaceModeChange(nextMode);
                  }
                }}
              />
            </div>
          )}
          {!isAgentMode && (
            <div className="flex flex-col gap-2 px-4">
              <button
                type="button"
                className="flex w-full items-center justify-start gap-2 py-1.5 pr-1 pl-0 text-sm text-[hsl(var(--maple-primary-strong))] transition-colors hover:text-[hsl(var(--maple-primary))] dark:text-[hsl(var(--maple-primary))] dark:hover:text-[hsl(var(--maple-primary-strong))]"
                onClick={() => void addChat()}
              >
                <SquarePenIcon className="h-4 w-4 shrink-0" />
                New Chat
              </button>
              <button
                type="button"
                className="flex w-full items-center justify-start gap-2 py-1.5 pr-1 pl-0 text-sm text-foreground hover:text-foreground/70 transition-colors"
                onClick={toggleSearch}
                aria-label={isSearchVisible ? "Hide search" : "Search chat history"}
              >
                <Search className="h-4 w-4" />
                Search
              </button>
            </div>
          )}
        </div>
        {!isAgentMode && isSelectionMode && (
          <div className="mb-2 space-y-2 px-4">
            <div className="flex items-center gap-2">
              <Button
                variant="ghost"
                size="icon"
                className="h-8 w-8"
                onClick={exitSelectionMode}
                aria-label="Cancel selection"
              >
                <X className="h-4 w-4" />
              </Button>
              <span className="text-sm font-medium">
                {selectedIds.size >= 20 ? "max" : selectedIds.size} selected
              </span>
            </div>
            <div className="flex w-full items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                className="h-8 flex-1"
                onClick={handleMoveSelected}
                disabled={selectedIds.size === 0}
              >
                <FolderInput className="mr-1 h-4 w-4" />
                Move
              </Button>
              <Button
                variant="destructive"
                size="sm"
                className="h-8 flex-1"
                onClick={handleDeleteSelected}
                disabled={selectedIds.size === 0}
              >
                <Trash2 className="mr-1 h-4 w-4" />
                Delete
              </Button>
            </div>
          </div>
        )}
        {!isAgentMode && isSearchVisible && (
          <div className="relative transition-all duration-200 ease-in-out px-4">
            <Input
              ref={searchInputRef}
              type="text"
              placeholder="Search chat titles..."
              className="pl-4 pr-8 h-9 rounded-full"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              onKeyDown={handleKeyDown}
              aria-label="Search chat titles"
            />
            {searchQuery && (
              <button
                type="button"
                onClick={clearSearch}
                className="absolute right-6 top-2.5 text-muted-foreground hover:text-foreground"
                aria-label="Clear search"
              >
                <XCircle className="h-4 w-4" />
              </button>
            )}
          </div>
        )}
        {isAgentMode ? (
          <div className="flex-1" />
        ) : (
          <div className="relative flex min-h-0 min-w-0 flex-1 flex-col">
            <nav
              ref={historyContainerRef}
              className="sidebar-scrollbar relative flex min-h-0 min-w-0 flex-1 flex-col overflow-y-auto overflow-x-clip pl-4 pr-2 pt-5 md:px-4"
            >
              <ChatHistoryList
                currentChatId={chatId}
                searchQuery={searchQuery}
                isMobile={isCompactLayout}
                isSelectionMode={isSelectionMode}
                onExitSelectionMode={exitSelectionMode}
                selectedIds={selectedIds}
                onSelectionChange={setSelectedIds}
                containerRef={historyContainerRef}
              />
              {/* Real empty tail so the last row sits in clear space — no overlay on hit targets */}
              <div aria-hidden className="min-h-[7.5rem] shrink-0 bg-transparent" />
            </nav>
            {/* Bottom scroll fade */}
            <div
              aria-hidden
              className="pointer-events-none absolute bottom-0 left-0 z-[8] h-8 w-[calc(100%-10px)] max-w-full bg-gradient-to-b from-transparent to-muted/75 dark:to-[hsl(var(--sidebar)/0.75)]"
            />
          </div>
        )}
        <div className="w-full border-t border-border/25 px-4 pb-4 pt-2">
          <AccountMenu />
        </div>
      </div>
    </div>
  );
}

export function SidebarToggle({ onToggle }: { onToggle: () => void }) {
  return (
    <button
      type="button"
      className="h-9 w-9 flex items-center justify-center text-foreground hover:text-foreground/70 transition-colors"
      onClick={onToggle}
      aria-label="Open sidebar"
    >
      <Menu className="h-4 w-4" />
    </button>
  );
}
