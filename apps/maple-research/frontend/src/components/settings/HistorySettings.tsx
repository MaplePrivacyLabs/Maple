import { useState } from "react";
import { useBlocker, useNavigate } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { useOpenSecret } from "@mapleai/sdk";
import { Loader2, Trash2 } from "lucide-react";
import { AlertDestructive } from "@/components/AlertDestructive";
import { Button } from "@/components/ui/button";
import { useChatRuntimeStore } from "@/contexts/ChatRuntimeContext";
import { useSettingsNavigationLock } from "@/contexts/SettingsNavigationLockContext";
import { useOpenAI } from "@/ai/useOpenAi";
import { clearAgentHistoryForUser } from "@/services/agentRuntimeService";
import {
  cancelChatResponseForHistoryDeletion,
  quiesceChatRuntimeRunsForHistoryDeletion,
  settleChatRuntimeAfterHistoryCancellation
} from "@/services/chatHistoryDeletionQuiescence";
import { beginAllChatRuntimeDeletionFence } from "@/services/chatRuntimeDeletionFence";
import { assertChatAccountCredential } from "@/services/chatAccountCredential";
import { SettingsPage, SettingsSection } from "./SettingsPage";

export function HistorySettings() {
  const os = useOpenSecret();
  const openai = useOpenAI();
  const runtimeStore = useChatRuntimeStore<unknown, unknown>();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [isConfirming, setIsConfirming] = useState(false);
  const [isDeleting, setIsDeleting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useBlocker({
    shouldBlockFn: () => isDeleting,
    disabled: !isDeleting,
    enableBeforeUnload: isDeleting
  });
  useSettingsNavigationLock(isDeleting);

  const handleDeleteHistory = async () => {
    const expectedUserId = os.auth.user?.user.id;
    setError(null);
    setIsDeleting(true);
    const releaseDeletionFence = beginAllChatRuntimeDeletionFence(runtimeStore);
    let operationBlock: Awaited<ReturnType<typeof clearAgentHistoryForUser>> | null = null;
    try {
      assertChatAccountCredential(expectedUserId);
      await quiesceChatRuntimeRunsForHistoryDeletion({
        store: runtimeStore,
        responseOwnershipClient: openai,
        cancelResponse: (responseId) => cancelChatResponseForHistoryDeletion(openai, responseId),
        settleCancelledRun: (key, runToken) =>
          settleChatRuntimeAfterHistoryCancellation(runtimeStore, key, runToken)
      });

      assertChatAccountCredential(expectedUserId);
      await os.deleteConversations();
      // Chat deletion is now confirmed (including the already-empty case).
      // Clear server-deleted transcripts and client-only queues even if the
      // separate Agent history cleanup below fails.
      runtimeStore.clearAll();

      operationBlock = await clearAgentHistoryForUser(expectedUserId);

      queryClient.invalidateQueries({ queryKey: ["conversations"] });
      queryClient.invalidateQueries({ queryKey: ["pinnedConversations"] });
      queryClient.invalidateQueries({ queryKey: ["projectConversations"] });
      queryClient.invalidateQueries({ queryKey: ["conversationProjects"] });
      queryClient.invalidateQueries({ queryKey: ["conversationProject"] });
      try {
        await navigate({ to: "/", ignoreBlocker: true });
      } catch (navigationError) {
        console.error("Chat history was deleted, but navigation failed:", navigationError);
        window.location.href = "/";
        return;
      }
      window.dispatchEvent(new CustomEvent("newchat", { detail: { projectId: null } }));
    } catch (deleteError) {
      console.error("Error deleting chat history:", deleteError);
      // Preserve unresolved response ownership so a retry can still issue the
      // server cancellation before any destructive request.
      setError("Maple could not delete all chat and task history. Please try again.");
    } finally {
      operationBlock?.release();
      releaseDeletionFence();
      setIsDeleting(false);
    }
  };

  return (
    <SettingsPage
      title="Chat and task history"
      description="Manage the chats and tasks stored in your private Maple workspace."
    >
      <SettingsSection
        title="Delete all chat and task history"
        description="This permanently removes every chat and task from your Maple workspace."
        tone="danger"
      >
        <div className="space-y-4">
          {error && (
            <AlertDestructive title="Chat and task history was not deleted" description={error} />
          )}
          {isConfirming ? (
            <div className="rounded-lg border border-destructive/40 bg-destructive/5 p-4">
              <p className="text-sm font-medium">Delete your entire chat and task history?</p>
              <p className="mt-1 text-sm text-muted-foreground">This action cannot be undone.</p>
              <div className="mt-4 flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
                <Button
                  type="button"
                  variant="outline"
                  onClick={() => setIsConfirming(false)}
                  disabled={isDeleting}
                >
                  Cancel
                </Button>
                <Button
                  type="button"
                  variant="destructive"
                  onClick={handleDeleteHistory}
                  disabled={isDeleting}
                >
                  {isDeleting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                  {isDeleting ? "Deleting..." : "Delete all history"}
                </Button>
              </div>
            </div>
          ) : (
            <Button type="button" variant="destructive" onClick={() => setIsConfirming(true)}>
              <Trash2 className="mr-2 h-4 w-4" />
              Delete all history
            </Button>
          )}
        </div>
      </SettingsSection>
    </SettingsPage>
  );
}
