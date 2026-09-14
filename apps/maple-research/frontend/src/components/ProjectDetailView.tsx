import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  CheckSquare,
  Folder,
  FolderInput,
  Loader2,
  MessageSquare,
  MoreHorizontal,
  Pencil,
  Pin,
  PinOff,
  SquarePen,
  Trash2
} from "lucide-react";
import { useOpenSecret, type Conversation } from "@mapleai/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Sidebar, SidebarToggle } from "@/components/Sidebar";
import { ResizableSidebarLayout } from "@/components/ResizableSidebarLayout";
import { useIsMobile, useIsLandscapeMobile } from "@/utils/utils";
import { useSelectedProjectState } from "@/state/useLocalState";
import { Button } from "@/components/ui/button";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from "@/components/ui/dialog";
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
import { ConversationProjectDialog } from "@/components/ConversationProjectDialog";
import { DeleteConversationProjectDialog } from "@/components/DeleteConversationProjectDialog";
import { RenameChatDialog } from "@/components/RenameChatDialog";
import { DeleteChatDialog } from "@/components/DeleteChatDialog";
import { BulkDeleteDialog } from "@/components/BulkDeleteDialog";
import { MoveChatsDialog } from "@/components/MoveChatsDialog";
import { listAllConversationProjects, listAllConversations } from "@/utils/paginatedLists";
import { usePersistentSidebarState } from "@/contexts/PersistentHomeNavigationContext";
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
  createChatHistoryEntryForDraft,
  type NewChatNavigationDetail
} from "@/services/chatRuntimeNavigation";
import {
  cancelChatResponseForHistoryDeletion,
  quiesceChatRuntimeRunsForHistoryDeletion,
  settleChatRuntimeAfterHistoryCancellation
} from "@/services/chatHistoryDeletionQuiescence";
import { assertChatAccountCredential } from "@/services/chatAccountCredential";

const PROJECT_PAGE_SIZE = 20;
const MAX_SELECTION = 20;

interface ProjectDetailViewProps {
  projectId: string;
}

function getConversationTitle(conversation: Conversation) {
  const title = conversation.metadata?.title;
  return typeof title === "string" && title.trim() ? title : "Untitled Chat";
}

function ProjectInstructionsDialog({
  open,
  onOpenChange,
  projectName,
  initialInstructions,
  onSave
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  projectName: string;
  initialInstructions: string | null;
  onSave: (instructions: string | null) => Promise<void>;
}) {
  const [instructions, setInstructions] = useState(initialInstructions ?? "");
  const [isSaving, setIsSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
      setInstructions(initialInstructions ?? "");
      setError(null);
    }
  }, [initialInstructions, open]);

  async function handleSubmit(event: React.FormEvent) {
    event.preventDefault();
    setError(null);
    setIsSaving(true);

    try {
      await onSave(instructions.trim() ? instructions : null);
      onOpenChange(false);
    } catch (saveError) {
      console.error("Failed to save project instructions:", saveError);
      setError("Failed to save project instructions. Please try again.");
    } finally {
      setIsSaving(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-[525px]">
        <DialogHeader>
          <DialogTitle>Custom Instructions</DialogTitle>
          <DialogDescription>These instructions apply to chats in {projectName}.</DialogDescription>
        </DialogHeader>
        <form onSubmit={handleSubmit} className="grid gap-4 py-4">
          {error ? (
            <Alert variant="destructive">
              <AlertDescription>{error}</AlertDescription>
            </Alert>
          ) : null}
          <div className="grid gap-2">
            <Label htmlFor="project-instructions">Instructions</Label>
            <Textarea
              id="project-instructions"
              value={instructions}
              onChange={(event) => setInstructions(event.target.value)}
              placeholder="e.g. Use concise bullet points. Focus on practical examples."
              className="min-h-[220px] resize-y"
              disabled={isSaving}
            />
            <p className="text-sm text-muted-foreground">
              Leave this empty to clear the project's custom instructions.
            </p>
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={isSaving}>
              {isSaving ? "Saving..." : "Save Instructions"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function ProjectDetailView({ projectId }: ProjectDetailViewProps) {
  const os = useOpenSecret();
  const openai = useOpenAI();
  const userId = os.auth.user?.user.id;
  const queryClient = useQueryClient();
  const isMobile = useIsMobile();
  const isLandscapeMobile = useIsLandscapeMobile();
  const isCompactLayout = isMobile || isLandscapeMobile;
  const { setSelectedProjectId } = useSelectedProjectState();
  const hasAuthUser = !!os.auth.user;
  const runtimeStore = useChatRuntimeStore<Conversation, unknown>();

  const [isSidebarOpen, setIsSidebarOpen] = usePersistentSidebarState(isCompactLayout);
  const [isSidebarTransitioning, setIsSidebarTransitioning] = useState(false);
  const wasLandscapeMobileRef = useRef(isLandscapeMobile);

  useEffect(() => {
    const enteredLandscapeMobile = isLandscapeMobile && !wasLandscapeMobileRef.current;
    wasLandscapeMobileRef.current = isLandscapeMobile;
    if (enteredLandscapeMobile && isSidebarOpen) {
      setIsSidebarOpen(false);
    }
  }, [isLandscapeMobile, isSidebarOpen, setIsSidebarOpen]);

  const [conversations, setConversations] = useState<Conversation[]>([]);
  const [hasMoreConversations, setHasMoreConversations] = useState(false);
  const [lastConversationId, setLastConversationId] = useState<string | undefined>();
  const [isLoadingConversations, setIsLoadingConversations] = useState(false);
  const [isLoadingMore, setIsLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [selectedChat, setSelectedChat] = useState<{ id: string; title: string } | null>(null);
  const [isRenameDialogOpen, setIsRenameDialogOpen] = useState(false);
  const [isDeleteDialogOpen, setIsDeleteDialogOpen] = useState(false);
  const [isBulkDeleteDialogOpen, setIsBulkDeleteDialogOpen] = useState(false);
  const [isMoveDialogOpen, setIsMoveDialogOpen] = useState(false);
  const [isBulkDeleting, setIsBulkDeleting] = useState(false);
  const [isBulkMoving, setIsBulkMoving] = useState(false);
  const [isRenameProjectDialogOpen, setIsRenameProjectDialogOpen] = useState(false);
  const [isDeleteProjectDialogOpen, setIsDeleteProjectDialogOpen] = useState(false);
  const [isInstructionsDialogOpen, setIsInstructionsDialogOpen] = useState(false);

  const toggleSidebar = useCallback(() => {
    setIsSidebarOpen((prev) => !prev);
  }, [setIsSidebarOpen]);

  useEffect(() => {
    setSelectedProjectId(projectId);
  }, [projectId, setSelectedProjectId]);

  const { data: project, isPending: isProjectPending } = useQuery({
    queryKey: ["conversationProject", projectId],
    queryFn: () => os.getConversationProject(projectId),
    enabled: !!projectId && hasAuthUser
  });

  const { data: conversationProjects = [] } = useQuery({
    queryKey: ["conversationProjects", userId],
    queryFn: () => listAllConversationProjects(os),
    enabled: !!userId
  });

  const isSelectionMode = selectedIds.size > 0;

  const loadConversations = useCallback(
    async (cursor?: string, append = false) => {
      if (!projectId || !hasAuthUser) return;

      if (append) {
        setIsLoadingMore(true);
      } else {
        setIsLoadingConversations(true);
      }

      setError(null);

      try {
        const response = await os.listConversations({
          limit: PROJECT_PAGE_SIZE,
          project_id: projectId,
          after: cursor
        });

        const nextData = response.data ?? [];
        setConversations((prev) => {
          if (!append) {
            return nextData;
          }

          const seen = new Set(prev.map((conversation) => conversation.id));
          const merged = [...prev];

          for (const conversation of nextData) {
            if (seen.has(conversation.id)) continue;
            seen.add(conversation.id);
            merged.push(conversation);
          }

          return merged;
        });
        setHasMoreConversations(response.has_more);
        setLastConversationId(response.last_id ?? nextData[nextData.length - 1]?.id);
      } catch (loadError) {
        console.error("Failed to load project conversations:", loadError);
        setError("Failed to load chats for this project. Please try again.");
      } finally {
        setIsLoadingConversations(false);
        setIsLoadingMore(false);
      }
    },
    [hasAuthUser, os, projectId]
  );

  useEffect(() => {
    if (!hasAuthUser) return;
    setSelectedIds(new Set());
    void loadConversations();
  }, [hasAuthUser, loadConversations]);

  useEffect(() => {
    if (!hasAuthUser) return;

    const handleConversationMetadataUpdated = (
      event: CustomEvent<{ conversationId: string; projectId?: string | null }>
    ) => {
      if (!Object.prototype.hasOwnProperty.call(event.detail, "projectId")) {
        return;
      }

      const isConversationVisible = conversations.some(
        (conversation) => conversation.id === event.detail.conversationId
      );

      if (event.detail.projectId === projectId || isConversationVisible) {
        void loadConversations();
      }
    };

    window.addEventListener(
      "conversationmetadataupdated",
      handleConversationMetadataUpdated as EventListener
    );
    return () => {
      window.removeEventListener(
        "conversationmetadataupdated",
        handleConversationMetadataUpdated as EventListener
      );
    };
  }, [conversations, hasAuthUser, loadConversations, projectId]);

  const invalidateConversationData = useCallback(async () => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ["conversations"] }),
      queryClient.invalidateQueries({ queryKey: ["pinnedConversations"] }),
      queryClient.invalidateQueries({ queryKey: ["projectConversations"] }),
      queryClient.invalidateQueries({ queryKey: ["conversationProjects"] }),
      queryClient.invalidateQueries({ queryKey: ["conversationProject", projectId] })
    ]);
  }, [projectId, queryClient]);

  const refreshProjectPage = useCallback(async () => {
    await invalidateConversationData();
    await loadConversations();
  }, [invalidateConversationData, loadConversations]);

  const updateRuntimeConversationProject = useCallback(
    (conversationId: string, targetProjectId: string | null) => {
      const key = createConversationChatKey(conversationId);
      if (runtimeStore.get(key)) {
        runtimeStore.update(key, (snapshot) => ({
          ...snapshot,
          conversation: snapshot.conversation
            ? { ...snapshot.conversation, project_id: targetProjectId }
            : null
        }));
      }
      runtimeStore.updateActivityGroup(key, targetProjectId);
    },
    [runtimeStore]
  );

  const handleStartNewChat = useCallback(() => {
    setSelectedProjectId(projectId);
    const params = new URLSearchParams(window.location.search);
    params.delete("project_id");
    params.delete("conversation_id");
    const draftRuntimeKey = resumeOrCreateChatDraftKey(runtimeStore, projectId);
    const chatEntry = createChatHistoryEntryForDraft(draftRuntimeKey);
    window.history.replaceState(
      chatEntry.historyState,
      "",
      params.toString() ? `/?${params.toString()}` : "/"
    );
    window.dispatchEvent(
      new CustomEvent<NewChatNavigationDetail>("newchat", {
        detail: { projectId, draftRuntimeKey: chatEntry.draftRuntimeKey }
      })
    );
    setTimeout(() => document.getElementById("message")?.focus(), 0);
  }, [projectId, runtimeStore, setSelectedProjectId]);

  const handleSelectConversation = useCallback(
    (conversation: Conversation) => {
      runtimeStore.markCompletionRead(createConversationChatKey(conversation.id));
      setSelectedProjectId(conversation.project_id ?? null);
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
    [runtimeStore, setSelectedProjectId]
  );

  const handleSaveProjectInstructions = useCallback(
    async (instructions: string | null) => {
      await os.updateConversationProject(projectId, { instructions });
      await invalidateConversationData();
    },
    [invalidateConversationData, os, projectId]
  );

  const handleRenameProject = useCallback(
    async (name: string) => {
      await os.updateConversationProject(projectId, { name });
      await invalidateConversationData();
    },
    [invalidateConversationData, os, projectId]
  );

  const handleDeleteProject = useCallback(async () => {
    const expectedUserId = os.auth.user?.user.id;
    const releaseDiscoveryFence = beginAllChatRuntimeDeletionFence(runtimeStore);
    let releaseDeletionFence: (() => void) | null = null;
    try {
      assertChatAccountCredential(expectedUserId);
      const projectConversationKeys = (
        await listAllConversations(os, { project_id: projectId })
      ).map((conversation) => createConversationChatKey(conversation.id));
      releaseDeletionFence = beginChatProjectRuntimeDeletionFence(
        runtimeStore,
        projectId,
        projectConversationKeys
      );
      releaseDiscoveryFence();

      await quiesceChatRuntimeRunsForHistoryDeletion({
        store: runtimeStore,
        keys: projectConversationKeys,
        activityGroupId: projectId,
        responseOwnershipClient: openai,
        cancelResponse: (responseId) => cancelChatResponseForHistoryDeletion(openai, responseId),
        settleCancelledRun: (key, runToken) =>
          settleChatRuntimeAfterHistoryCancellation(runtimeStore, key, runToken)
      });

      assertChatAccountCredential(expectedUserId);
      await os.deleteConversationProject(projectId);
      for (const key of projectConversationKeys) runtimeStore.delete(key);
      runtimeStore.deleteActivityGroup(projectId);
      await invalidateConversationData();
      setSelectedProjectId(null);
      const draftRuntimeKey = rootChatDraftKeyAfterProjectDeletion(runtimeStore);
      const chatEntry = createChatHistoryEntryForDraft(draftRuntimeKey);
      window.history.replaceState(chatEntry.historyState, "", "/");
      window.dispatchEvent(
        new CustomEvent<NewChatNavigationDetail>("newchat", {
          detail: { projectId: null, draftRuntimeKey: chatEntry.draftRuntimeKey }
        })
      );
      window.dispatchEvent(new Event("projectselected"));
    } finally {
      releaseDeletionFence?.();
      releaseDiscoveryFence();
    }
  }, [invalidateConversationData, openai, os, projectId, runtimeStore, setSelectedProjectId]);

  const handleRenameConversation = useCallback(
    async (conversationId: string, newTitle: string) => {
      await os.updateConversation(conversationId, { title: newTitle });
      await refreshProjectPage();
    },
    [os, refreshProjectPage]
  );

  const handleDeleteConversation = useCallback(
    async (conversationId: string) => {
      const expectedUserId = os.auth.user?.user.id;
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
        await os.deleteConversation(conversationId);
        runtimeStore.delete(createConversationChatKey(conversationId));
        setSelectedIds((prev) => {
          const next = new Set(prev);
          next.delete(conversationId);
          return next;
        });
        await refreshProjectPage();
      } finally {
        releaseDeletionFence();
      }
    },
    [openai, os, refreshProjectPage, runtimeStore]
  );

  const handleToggleConversationPin = useCallback(
    async (conversation: Conversation) => {
      await os.updateConversation(conversation.id, undefined, {
        pinned: !conversation.pinned
      });
      await refreshProjectPage();
    },
    [os, refreshProjectPage]
  );

  const handleMoveConversationToProject = useCallback(
    async (conversation: Conversation, targetProjectId: string | null) => {
      await os.batchUpdateConversationProject([conversation.id], targetProjectId);
      updateRuntimeConversationProject(conversation.id, targetProjectId);
      setSelectedIds((prev) => {
        const next = new Set(prev);
        next.delete(conversation.id);
        return next;
      });
      await refreshProjectPage();
    },
    [os, refreshProjectPage, updateRuntimeConversationProject]
  );

  const handleBulkDelete = useCallback(async () => {
    if (selectedIds.size === 0) return;

    setIsBulkDeleting(true);
    setError(null);
    const releaseDeletionFences = Array.from(selectedIds, (id) =>
      beginChatRuntimeDeletionFence(runtimeStore, createConversationChatKey(id))
    );
    try {
      const expectedUserId = os.auth.user?.user.id;
      assertChatAccountCredential(expectedUserId);
      const idsToDelete = Array.from(selectedIds);
      await quiesceChatRuntimeRunsForHistoryDeletion({
        store: runtimeStore,
        keys: idsToDelete.map((id) => createConversationChatKey(id)),
        responseOwnershipClient: openai,
        cancelResponse: (responseId) => cancelChatResponseForHistoryDeletion(openai, responseId),
        settleCancelledRun: (key, runToken) =>
          settleChatRuntimeAfterHistoryCancellation(runtimeStore, key, runToken)
      });
      assertChatAccountCredential(expectedUserId);
      const result = await os.batchDeleteConversations(idsToDelete);
      for (const item of result.data) {
        if (item.deleted) runtimeStore.delete(createConversationChatKey(item.id));
      }
      setSelectedIds(new Set());
      setIsBulkDeleteDialogOpen(false);
      await refreshProjectPage();
    } catch (error) {
      console.error("Error bulk deleting chats:", error);
      setError("Failed to delete selected chats. Please try again.");
    } finally {
      for (const releaseDeletionFence of releaseDeletionFences) releaseDeletionFence();
      setIsBulkDeleting(false);
    }
  }, [openai, os, refreshProjectPage, runtimeStore, selectedIds]);

  const handleMoveSelectedConversations = useCallback(
    async (targetProjectId: string | null) => {
      if (selectedIds.size === 0) return;

      setIsBulkMoving(true);
      try {
        const ids = Array.from(selectedIds);
        await os.batchUpdateConversationProject(ids, targetProjectId);
        for (const id of ids) updateRuntimeConversationProject(id, targetProjectId);
        setSelectedIds(new Set());
        await refreshProjectPage();
      } catch (error) {
        console.error("Error moving selected chats:", error);
        throw error;
      } finally {
        setIsBulkMoving(false);
      }
    },
    [os, refreshProjectPage, selectedIds, updateRuntimeConversationProject]
  );

  const toggleSelection = useCallback((conversationId: string) => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (next.has(conversationId)) {
        next.delete(conversationId);
      } else {
        if (next.size >= MAX_SELECTION) {
          return prev;
        }
        next.add(conversationId);
      }
      return next;
    });
  }, []);

  const currentInstructionsPreview = useMemo(() => {
    if (!project?.instructions?.trim()) {
      return "Set shared custom instructions that apply to chats in this project.";
    }

    return project.instructions;
  }, [project?.instructions]);

  if (isProjectPending && !project) {
    return (
      <ResizableSidebarLayout
        isCompactLayout={isCompactLayout}
        isOpen={isSidebarOpen}
        mode="chat"
        onOpenChange={setIsSidebarOpen}
        onTransitionChange={setIsSidebarTransitioning}
        sidebar={<Sidebar isOpen={isSidebarOpen} onToggle={toggleSidebar} />}
        userId={userId}
      >
        <div className="relative flex min-h-0 min-w-0 flex-1 items-center justify-center bg-background">
          {!isSidebarOpen && !isSidebarTransitioning ? (
            <div className="fixed left-4 top-[9.5px] z-20">
              <SidebarToggle onToggle={toggleSidebar} />
            </div>
          ) : null}
          <p className="text-sm text-muted-foreground">Loading project...</p>
        </div>
      </ResizableSidebarLayout>
    );
  }

  return (
    <ResizableSidebarLayout
      isCompactLayout={isCompactLayout}
      isOpen={isSidebarOpen}
      mode="chat"
      onOpenChange={setIsSidebarOpen}
      onTransitionChange={setIsSidebarTransitioning}
      sidebar={<Sidebar isOpen={isSidebarOpen} onToggle={toggleSidebar} />}
      userId={userId}
    >
      <div className="relative flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden bg-background">
        {!isSidebarOpen && !isSidebarTransitioning ? (
          <div className="fixed left-4 top-[9.5px] z-20">
            <SidebarToggle onToggle={toggleSidebar} />
          </div>
        ) : null}

        <div className="h-14 flex items-center px-4">
          <div className="relative flex flex-1 items-center justify-center">
            <h1 className="max-w-[20rem] truncate text-base font-medium text-foreground">
              {project?.name ?? "Project"}
            </h1>
            {project ? (
              <div className="absolute right-0">
                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <Button variant="ghost" size="icon" className="h-9 w-9">
                      <MoreHorizontal className="h-4 w-4" />
                    </Button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent align="end">
                    <DropdownMenuItem onClick={() => setIsRenameProjectDialogOpen(true)}>
                      <Pencil className="mr-2 h-4 w-4" />
                      Rename Project
                    </DropdownMenuItem>
                    <DropdownMenuItem onClick={() => setIsDeleteProjectDialogOpen(true)}>
                      <Trash2 className="mr-2 h-4 w-4" />
                      Delete Project
                    </DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              </div>
            ) : null}
          </div>
        </div>

        <div className="flex-1 min-h-0 overflow-y-auto overflow-x-hidden">
          <div className="mx-auto flex w-full max-w-6xl flex-col gap-6 px-4 py-6 md:px-8">
            {error ? (
              <Alert variant="destructive">
                <AlertDescription>{error}</AlertDescription>
              </Alert>
            ) : null}

            {!project && !isProjectPending ? (
              <div className="rounded-xl border bg-background/60 p-6 text-sm text-muted-foreground">
                Project not found.
              </div>
            ) : project ? (
              <div className="grid grid-cols-[minmax(0,1fr)] gap-6 lg:grid-cols-[minmax(0,1fr)_320px]">
                <div className="contents lg:block lg:min-w-0 lg:overflow-hidden lg:space-y-4">
                  <button
                    type="button"
                    onClick={handleStartNewChat}
                    className="order-1 flex h-11 w-full items-center gap-2 rounded-lg border bg-background px-3 text-left text-sm transition-colors hover:border-primary/40 hover:bg-accent/20 lg:order-none"
                  >
                    <SquarePen className="h-4 w-4 text-muted-foreground" />
                    <span className="truncate text-muted-foreground">
                      New chat in {project.name}
                    </span>
                  </button>

                  <div className="order-3 min-w-0 overflow-hidden rounded-xl border bg-background/60 lg:order-none">
                    <div className="flex flex-wrap items-center justify-between gap-3 border-b px-4 py-3">
                      <h2 className="font-medium">Chats</h2>
                      {isSelectionMode ? (
                        <div className="flex flex-wrap items-center gap-2">
                          <span className="text-sm text-muted-foreground">
                            {selectedIds.size}/{MAX_SELECTION} selected
                          </span>
                          <Button
                            type="button"
                            variant="outline"
                            size="sm"
                            onClick={() => setIsMoveDialogOpen(true)}
                          >
                            <FolderInput className="mr-1 h-4 w-4" />
                            Move
                          </Button>
                          <Button
                            type="button"
                            variant="destructive"
                            size="sm"
                            onClick={() => setIsBulkDeleteDialogOpen(true)}
                          >
                            <Trash2 className="mr-1 h-4 w-4" />
                            Delete
                          </Button>
                          <Button
                            type="button"
                            variant="ghost"
                            size="sm"
                            onClick={() => setSelectedIds(new Set())}
                          >
                            Cancel
                          </Button>
                        </div>
                      ) : null}
                    </div>

                    {isLoadingConversations ? (
                      <div className="flex items-center gap-2 px-4 py-8 text-sm text-muted-foreground">
                        <Loader2 className="h-4 w-4 animate-spin" />
                        Loading chats...
                      </div>
                    ) : conversations.length === 0 ? (
                      <div className="px-4 py-8 text-sm text-muted-foreground">
                        No chats in this project yet.
                      </div>
                    ) : (
                      <div className="flex flex-col">
                        {conversations.map((conversation) => {
                          const isSelected = selectedIds.has(conversation.id);

                          return (
                            <div
                              key={conversation.id}
                              className={`group relative border-b last:border-b-0 ${
                                isSelected ? "bg-primary/5" : ""
                              }`}
                            >
                              <button
                                type="button"
                                onClick={() => {
                                  if (isSelectionMode) {
                                    toggleSelection(conversation.id);
                                  } else {
                                    handleSelectConversation(conversation);
                                  }
                                }}
                                className="flex w-full items-start gap-3 px-4 py-3 text-left transition-colors hover:bg-accent/30"
                              >
                                {isSelectionMode ? (
                                  <div
                                    className={`mt-1 flex h-4 w-4 shrink-0 items-center justify-center rounded border ${
                                      isSelected
                                        ? "border-primary bg-primary text-primary-foreground"
                                        : "border-border"
                                    }`}
                                  >
                                    {isSelected ? <CheckSquare className="h-3 w-3" /> : null}
                                  </div>
                                ) : (
                                  <div className="mt-0.5 flex h-8 w-8 items-center justify-center rounded-md bg-muted">
                                    <MessageSquare className="h-4 w-4 text-muted-foreground" />
                                  </div>
                                )}

                                <div className="min-w-0 flex-1 pr-10">
                                  <div className="flex min-w-0 items-center gap-2">
                                    <span className="truncate font-medium">
                                      {getConversationTitle(conversation)}
                                    </span>
                                    {conversation.pinned ? (
                                      <Pin className="h-3.5 w-3.5 text-muted-foreground" />
                                    ) : null}
                                  </div>
                                  <div className="mt-1 text-xs text-muted-foreground">
                                    {new Date(
                                      conversation.last_activity_at * 1000
                                    ).toLocaleDateString()}
                                  </div>
                                </div>
                              </button>

                              {!isSelectionMode ? (
                                <DropdownMenu>
                                  <DropdownMenuTrigger asChild>
                                    <Button
                                      type="button"
                                      variant="ghost"
                                      size="icon"
                                      className="absolute right-2 top-2 h-8 w-8 text-foreground/40 transition-colors hover:text-foreground group-hover:text-foreground focus-visible:text-foreground"
                                      onClick={(event) => {
                                        event.preventDefault();
                                        event.stopPropagation();
                                      }}
                                    >
                                      <MoreHorizontal className="h-4 w-4" />
                                    </Button>
                                  </DropdownMenuTrigger>
                                  <DropdownMenuContent align="end">
                                    <DropdownMenuItem
                                      onClick={() => toggleSelection(conversation.id)}
                                    >
                                      <CheckSquare className="mr-2 h-4 w-4" />
                                      Select
                                    </DropdownMenuItem>
                                    <DropdownMenuItem
                                      onClick={() => void handleToggleConversationPin(conversation)}
                                    >
                                      {conversation.pinned ? (
                                        <PinOff className="mr-2 h-4 w-4" />
                                      ) : (
                                        <Pin className="mr-2 h-4 w-4" />
                                      )}
                                      {conversation.pinned ? "Unpin Chat" : "Pin Chat"}
                                    </DropdownMenuItem>
                                    <DropdownMenuSub>
                                      <DropdownMenuSubTrigger>
                                        <FolderInput className="mr-2 h-4 w-4" />
                                        Move to Project
                                      </DropdownMenuSubTrigger>
                                      <DropdownMenuSubContent>
                                        <DropdownMenuItem
                                          onClick={() =>
                                            void handleMoveConversationToProject(conversation, null)
                                          }
                                        >
                                          <Folder className="mr-2 h-4 w-4" />
                                          No project
                                        </DropdownMenuItem>
                                        <DropdownMenuSeparator />
                                        {conversationProjects.map((conversationProject) => (
                                          <DropdownMenuItem
                                            key={conversationProject.id}
                                            onClick={() =>
                                              void handleMoveConversationToProject(
                                                conversation,
                                                conversationProject.id
                                              )
                                            }
                                          >
                                            <Folder className="mr-2 h-4 w-4" />
                                            {conversationProject.name}
                                          </DropdownMenuItem>
                                        ))}
                                      </DropdownMenuSubContent>
                                    </DropdownMenuSub>
                                    <DropdownMenuItem
                                      onClick={() => {
                                        setSelectedChat({
                                          id: conversation.id,
                                          title: getConversationTitle(conversation)
                                        });
                                        setIsRenameDialogOpen(true);
                                      }}
                                    >
                                      <Pencil className="mr-2 h-4 w-4" />
                                      Rename Chat
                                    </DropdownMenuItem>
                                    <DropdownMenuItem
                                      onClick={() => {
                                        setSelectedChat({
                                          id: conversation.id,
                                          title: getConversationTitle(conversation)
                                        });
                                        setIsDeleteDialogOpen(true);
                                      }}
                                    >
                                      <Trash2 className="mr-2 h-4 w-4" />
                                      Delete Chat
                                    </DropdownMenuItem>
                                  </DropdownMenuContent>
                                </DropdownMenu>
                              ) : null}
                            </div>
                          );
                        })}
                      </div>
                    )}

                    {hasMoreConversations ? (
                      <div className="border-t px-4 py-3">
                        <Button
                          type="button"
                          variant="outline"
                          size="sm"
                          onClick={() => void loadConversations(lastConversationId, true)}
                          disabled={isLoadingMore || !lastConversationId}
                        >
                          {isLoadingMore ? "Loading..." : "Load more"}
                        </Button>
                      </div>
                    ) : null}
                  </div>
                </div>

                <div className="order-2 space-y-4 lg:order-none">
                  <button
                    type="button"
                    onClick={() => setIsInstructionsDialogOpen(true)}
                    className="w-full rounded-xl border bg-background/60 p-4 text-left transition-colors hover:border-primary/40 hover:bg-accent/20"
                  >
                    <div className="mb-3 flex items-center justify-between gap-3">
                      <div>
                        <h2 className="font-medium">Custom Instructions</h2>
                        <p className="mt-1 text-sm text-muted-foreground">
                          Shared instructions for chats in {project.name}
                        </p>
                      </div>
                      <MoreHorizontal className="h-4 w-4 text-muted-foreground" />
                    </div>
                    <p className="whitespace-pre-wrap text-sm text-muted-foreground">
                      {currentInstructionsPreview}
                    </p>
                  </button>
                </div>
              </div>
            ) : null}
          </div>
        </div>
      </div>

      {project ? (
        <>
          <ProjectInstructionsDialog
            open={isInstructionsDialogOpen}
            onOpenChange={setIsInstructionsDialogOpen}
            projectName={project.name}
            initialInstructions={project.instructions}
            onSave={handleSaveProjectInstructions}
          />

          <ConversationProjectDialog
            open={isRenameProjectDialogOpen}
            onOpenChange={setIsRenameProjectDialogOpen}
            mode="rename"
            initialName={project.name}
            onSubmit={handleRenameProject}
          />

          <DeleteConversationProjectDialog
            open={isDeleteProjectDialogOpen}
            onOpenChange={setIsDeleteProjectDialogOpen}
            projectName={project.name}
            onConfirm={handleDeleteProject}
          />
        </>
      ) : null}

      {selectedChat ? (
        <>
          <RenameChatDialog
            open={isRenameDialogOpen}
            onOpenChange={(open) => {
              setIsRenameDialogOpen(open);
              if (!open) {
                setSelectedChat(null);
              }
            }}
            chatId={selectedChat.id}
            currentTitle={selectedChat.title}
            onRename={handleRenameConversation}
          />
          <DeleteChatDialog
            open={isDeleteDialogOpen}
            onOpenChange={(open) => {
              setIsDeleteDialogOpen(open);
              if (!open) {
                setSelectedChat(null);
              }
            }}
            chatTitle={selectedChat.title}
            onConfirm={() => void handleDeleteConversation(selectedChat.id)}
          />
        </>
      ) : null}

      <BulkDeleteDialog
        open={isBulkDeleteDialogOpen}
        onOpenChange={setIsBulkDeleteDialogOpen}
        onConfirm={() => void handleBulkDelete()}
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
    </ResizableSidebarLayout>
  );
}
