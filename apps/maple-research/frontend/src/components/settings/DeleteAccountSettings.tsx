import { suspendAppleBillingForAccount } from "@/billing/appleBillingLifecycle";
import { useRef, useState } from "react";
import { Link, useBlocker } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { generateSecureSecret, hashSecret, useOpenSecret } from "@mapleai/sdk";
import { AlertTriangle, Loader2 } from "lucide-react";
import { shouldWarnBeforeAccountDeletion } from "@/billing/billingAccess";
import { getBillingService } from "@/billing/billingService";
import { Alert, AlertDescription } from "@/components/ui/alert";
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useSettingsNavigationLock } from "@/contexts/SettingsNavigationLockContext";
import { useChatRuntimeStore } from "@/contexts/ChatRuntimeContext";
import { useOpenAI } from "@/ai/useOpenAi";
import { beginAllChatRuntimeDeletionFence } from "@/services/chatRuntimeDeletionFence";
import { assertChatAccountCredential } from "@/services/chatAccountCredential";
import { accountDeletionBillingErrorMessage } from "@/services/accountDeletionError";
import {
  cancelChatResponseForHistoryDeletion,
  quiesceChatRuntimeRunsForHistoryDeletion,
  settleChatRuntimeAfterHistoryCancellation
} from "@/services/chatHistoryDeletionQuiescence";
import {
  clearAgentDataForUser,
  clearMapleApiAuthForUser,
  restoreMapleApiAuthForUser
} from "@/services/agentRuntimeService";
import { resetWorkspaceModePreference } from "@/services/workspaceModePreference";
import { useBillingState } from "@/state/useLocalState";
import { SettingsPage, SettingsSection } from "./SettingsPage";

export function DeleteAccountSettings() {
  const os = useOpenSecret();
  const openai = useOpenAI();
  const queryClient = useQueryClient();
  const { billingStatus } = useBillingState();
  const runtimeStore = useChatRuntimeStore<unknown, unknown>();
  const [step, setStep] = useState<"request" | "confirm">("request");
  const [confirmationCode, setConfirmationCode] = useState("");
  const [secret, setSecret] = useState("");
  const [confirmText, setConfirmText] = useState("");
  const [isLoading, setIsLoading] = useState(false);
  const [isAccountDeleted, setIsAccountDeleted] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showPaidPlanWarning, setShowPaidPlanWarning] = useState(false);
  const cleanupBlockRef = useRef<Awaited<ReturnType<typeof clearAgentDataForUser>> | null>(null);
  const allowProgrammaticUnloadRef = useRef(false);
  const paidPlanWarningAcknowledgedRef = useRef(false);
  const isNavigationLocked = isLoading || isAccountDeleted;

  useBlocker({
    shouldBlockFn: () => isNavigationLocked,
    disabled: !isNavigationLocked,
    enableBeforeUnload: () => isNavigationLocked && !allowProgrammaticUnloadRef.current
  });
  useSettingsNavigationLock(isNavigationLocked);

  const handleRequestDeletion = async () => {
    if (confirmText !== "DELETE") {
      setError("Please type DELETE to confirm.");
      return;
    }

    setIsLoading(true);
    setError(null);
    try {
      const expectedUserId = os.auth.user?.user.id;
      assertChatAccountCredential(expectedUserId);
      const generatedSecret = generateSecureSecret();
      const hashedSecret = await hashSecret(generatedSecret);
      assertChatAccountCredential(expectedUserId);
      await os.requestAccountDeletion(hashedSecret);
      setSecret(generatedSecret);
      setStep("confirm");
    } catch (requestError) {
      console.error(requestError);
      setError("Failed to request account deletion. Please try again.");
    } finally {
      setIsLoading(false);
    }
  };

  const handleConfirmDeletion = async () => {
    setIsLoading(true);
    setError(null);
    const releaseChatFence = beginAllChatRuntimeDeletionFence(runtimeStore);
    let releaseAppleBilling = () => {};
    let deletionConfirmed = isAccountDeleted;
    let agentDataCleared = cleanupBlockRef.current !== null;
    let proxyReset = false;
    let nativeAuthCleared = false;
    const userId = os.auth.user?.user.id;

    try {
      assertChatAccountCredential(userId);
      releaseAppleBilling = suspendAppleBillingForAccount(userId);
      await quiesceChatRuntimeRunsForHistoryDeletion({
        store: runtimeStore,
        responseOwnershipClient: openai,
        cancelResponse: (responseId) => cancelChatResponseForHistoryDeletion(openai, responseId),
        settleCancelledRun: (key, runToken) =>
          settleChatRuntimeAfterHistoryCancellation(runtimeStore, key, runToken)
      });

      if (!cleanupBlockRef.current) {
        // Clear local Agent data before the irreversible remote deletion.
        cleanupBlockRef.current = await clearAgentDataForUser(userId);
        agentDataCleared = true;
      }

      // Proxy credential reset is required before remote deletion so a crash
      // cannot leave a deleted account's key on disk.
      const { proxyService } = await import("@/services/proxyService");
      await proxyService.stopAndResetProxy(userId, (name) => {
        assertChatAccountCredential(userId);
        return os.deleteApiKey(name);
      });
      proxyReset = true;

      await clearMapleApiAuthForUser(userId);
      nativeAuthCleared = true;

      if (!deletionConfirmed) {
        assertChatAccountCredential(userId);
        await os.confirmAccountDeletion(confirmationCode, secret);
        deletionConfirmed = true;
        setIsAccountDeleted(true);
        cleanupBlockRef.current.retainUntilNextSession();
      }

      // The remote account no longer owns any renderable client state. Clear
      // queued messages and attachment resources before credential cleanup,
      // which can fail independently during a cross-tab account transition.
      runtimeStore.clearAll();

      resetWorkspaceModePreference();

      try {
        getBillingService().clearToken();
      } catch (clearError) {
        console.error("Error clearing billing token:", clearError);
        sessionStorage.removeItem("maple_billing_token");
      }

      await os.signOut();
      queryClient.clear();
      allowProgrammaticUnloadRef.current = true;
      window.location.href = "/";
    } catch (confirmError) {
      console.error("Account deletion confirmation or local cleanup failed.");
      // If remote deletion did not happen, the authenticated user must be able
      // to start a fresh Agent runtime after this attempt.
      if (!deletionConfirmed) {
        releaseChatFence();
        if (nativeAuthCleared) {
          try {
            await restoreMapleApiAuthForUser(userId);
          } catch (restoreError) {
            console.error("Error restoring Maple API authentication:", restoreError);
          }
        }
        cleanupBlockRef.current?.release();
        cleanupBlockRef.current = null;
      }
      setError(
        !agentDataCleared
          ? "Maple could not safely stop and clear local Agent Mode data. Your account was not deleted; please retry."
          : deletionConfirmed
            ? "Your account data was deleted, but Maple could not finish resetting local credentials or signing out. Please retry."
            : !nativeAuthCleared
              ? "Local Agent Mode history was cleared, but Maple could not reset its native authentication. Your account was not deleted; please retry."
              : !proxyReset
                ? "Local Agent Mode history was cleared, but Maple could not reset its proxy credentials. Your account was not deleted; please retry."
                : (accountDeletionBillingErrorMessage(confirmError) ??
                  "Local Agent Mode history was cleared, but account deletion was not confirmed. Verify the code and retry if you still want to delete your account.")
      );
      setIsLoading(false);
    } finally {
      // Keep purchases fenced if remote deletion succeeded but sign-out failed.
      if (!deletionConfirmed) releaseAppleBilling();
    }
  };

  const handleDeleteAccountClick = () => {
    if (step !== "request") {
      void handleConfirmDeletion();
      return;
    }

    // Billing does not currently expose cancel-at-period-end, so paid users can
    // still proceed if they already canceled and are waiting for the period to end.
    if (!paidPlanWarningAcknowledgedRef.current && shouldWarnBeforeAccountDeletion(billingStatus)) {
      setShowPaidPlanWarning(true);
      return;
    }

    void handleRequestDeletion();
  };

  const handleProceedAnyway = () => {
    paidPlanWarningAcknowledgedRef.current = true;
    setShowPaidPlanWarning(false);
    void handleRequestDeletion();
  };

  return (
    <SettingsPage
      title={step === "request" ? "Delete account" : "Confirm account deletion"}
      description={
        step === "request"
          ? "Permanently remove your Maple account and all associated data."
          : "Complete the deletion request using the code sent to your email. Submitting it clears local Agent Mode history and proxy credentials before the final account deletion request, even if the code is rejected."
      }
    >
      <SettingsSection tone="danger">
        <div className="space-y-5">
          <Alert variant="destructive">
            <AlertTriangle className="h-4 w-4" />
            <AlertDescription>
              This action cannot be undone. You will lose access to paid features, chat and task
              history, and settings.
            </AlertDescription>
          </Alert>

          {error && (
            <Alert variant="destructive">
              <AlertDescription>{error}</AlertDescription>
            </Alert>
          )}

          {step === "request" ? (
            <div className="grid gap-2">
              <Label htmlFor="settings-delete-confirmation">Type DELETE to confirm</Label>
              <Input
                id="settings-delete-confirmation"
                value={confirmText}
                onChange={(event) => setConfirmText(event.target.value)}
                autoComplete="off"
              />
            </div>
          ) : (
            <div className="grid gap-2">
              <Label htmlFor="settings-deletion-code">Confirmation code</Label>
              <Input
                id="settings-deletion-code"
                value={confirmationCode}
                onChange={(event) => setConfirmationCode(event.target.value)}
                placeholder="Enter code from email"
                autoComplete="one-time-code"
              />
            </div>
          )}

          <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
            {isNavigationLocked ? (
              <Button type="button" variant="outline" disabled>
                Cancel
              </Button>
            ) : (
              <Button asChild variant="outline">
                <Link to="/settings/account" replace>
                  Cancel
                </Link>
              </Button>
            )}
            <Button
              type="button"
              variant="destructive"
              onClick={handleDeleteAccountClick}
              disabled={
                isLoading ||
                (step === "request"
                  ? confirmText !== "DELETE"
                  : !isAccountDeleted && !confirmationCode.trim())
              }
            >
              {isLoading && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              {isLoading
                ? "Processing..."
                : isAccountDeleted
                  ? "Finish cleanup"
                  : step === "request"
                    ? "Delete account"
                    : "Confirm deletion"}
            </Button>
          </div>
        </div>
      </SettingsSection>

      <AlertDialog open={showPaidPlanWarning} onOpenChange={setShowPaidPlanWarning}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Cancel your plan first?</AlertDialogTitle>
            <AlertDialogDescription>
              This account is still on a paid plan. Cancel the subscription from the manage plan
              page before deleting the account. If you already canceled and are waiting for the
              period to end, you can proceed anyway.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <Button type="button" variant="outline" onClick={handleProceedAnyway}>
              Proceed anyway
            </Button>
            <Button asChild>
              <Link to="/settings/billing" replace>
                Manage plan
              </Link>
            </Button>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </SettingsPage>
  );
}
