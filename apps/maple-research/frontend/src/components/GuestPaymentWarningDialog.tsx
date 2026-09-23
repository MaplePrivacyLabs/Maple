import { suspendAppleBillingForAccount } from "@/billing/appleBillingLifecycle";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle
} from "@/components/ui/dialog";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import { AlertTriangle, LogOut, CreditCard } from "lucide-react";
import {
  clearMapleApiAuthForUser,
  restoreMapleApiAuthForUser,
  stopAgentRuntimeForUser
} from "@/services/agentRuntimeService";
import { resetWorkspaceModePreference } from "@/services/workspaceModePreference";
import { useState } from "react";
import { getBillingService } from "@/billing/billingService";
import { useChatRuntimeStore } from "@/contexts/ChatRuntimeContext";
import { beginAllChatRuntimeDeletionFence } from "@/services/chatRuntimeDeletionFence";
import { assertChatAccountCredential } from "@/services/chatAccountCredential";
import { isIOS } from "@/utils/platform";

interface GuestPaymentWarningDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function GuestPaymentWarningDialog({ open, onOpenChange }: GuestPaymentWarningDialogProps) {
  const navigate = useNavigate();
  const os = useOpenSecret();
  const queryClient = useQueryClient();
  const runtimeStore = useChatRuntimeStore<unknown, unknown>();
  const [isLoggingOut, setIsLoggingOut] = useState(false);
  const [logoutError, setLogoutError] = useState<string | null>(null);

  const handleGoToPricing = () => {
    navigate({ to: "/pricing" });
  };

  const handleLogout = async () => {
    setLogoutError(null);
    setIsLoggingOut(true);
    const releaseChatFence = beginAllChatRuntimeDeletionFence(runtimeStore);
    let operationBlock: Awaited<ReturnType<typeof stopAgentRuntimeForUser>> | null = null;
    let signedOut = false;
    let nativeAuthCleared = false;
    const userId = os.auth.user?.user.id;

    try {
      assertChatAccountCredential(userId);
    } catch (error) {
      console.error("Account changed before sign out:", error);
      releaseChatFence();
      setLogoutError("Your account changed in another window. Refresh Maple before signing out.");
      setIsLoggingOut(false);
      return;
    }

    const releaseAppleBilling = suspendAppleBillingForAccount(userId);
    try {
      operationBlock = await stopAgentRuntimeForUser(userId);
    } catch (error) {
      releaseAppleBilling();
      console.error("Error stopping Agent Mode:", error);
      releaseChatFence();
      setLogoutError("Maple couldn't stop Agent Mode. Please try logging out again.");
      setIsLoggingOut(false);
      return;
    }

    try {
      assertChatAccountCredential(userId);
      // Credential reset is a required part of logout.
      const { proxyService } = await import("@/services/proxyService");
      await proxyService.stopAndResetProxy(userId, (name) => {
        assertChatAccountCredential(userId);
        return os.deleteApiKey(name);
      });

      assertChatAccountCredential(userId);
      // Third-party billing tokens outlive the OpenSecret browser session. If
      // one survives logout, the next account can briefly query billing as the
      // previous user until that token expires.
      try {
        getBillingService().clearToken();
      } catch {
        sessionStorage.removeItem("maple_billing_token");
      }

      await clearMapleApiAuthForUser(userId);
      nativeAuthCleared = true;
      assertChatAccountCredential(userId);
      await os.signOut();
      signedOut = true;
      resetWorkspaceModePreference();
      queryClient.clear();
    } catch (error) {
      console.error("Error during sign out:", error);
      setLogoutError(
        "Maple couldn't securely reset Agent Mode or finish logging out. Please try again."
      );
    } finally {
      releaseAppleBilling();
      if (!signedOut) {
        releaseChatFence();
        if (nativeAuthCleared) {
          try {
            await restoreMapleApiAuthForUser(userId);
          } catch (error) {
            console.error("Error restoring Maple API authentication:", error);
          }
        }
        operationBlock.release();
        setIsLoggingOut(false);
      } else {
        operationBlock.retainUntilNextSession();
      }
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        className="max-w-[calc(100vw-2rem)] sm:max-w-[425px] [&>button]:hidden"
        onInteractOutside={(e) => e.preventDefault()}
        onEscapeKeyDown={(e) => e.preventDefault()}
      >
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2 text-maple-warning">
            <AlertTriangle className="w-5 h-5" />
            Subscription Required
          </DialogTitle>
          <DialogDescription>
            Anonymous accounts require a paid subscription to use Maple AI.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-4">
          <div className="space-y-3 rounded-lg border border-maple-warning/40 bg-maple-warning/10 p-4">
            <p className="text-sm font-medium">
              Your anonymous account is not activated yet and cannot use the chat feature.
            </p>
            <p className="text-sm text-muted-foreground">
              {isIOS()
                ? "You can subscribe with Apple or restore an existing Apple purchase. Keep your Maple Account ID and password so you can sign in again."
                : "To start chatting with Maple AI, you need to subscribe to a paid plan. Anonymous accounts must pay for a full year using Bitcoin or redeem a subscription pass."}
            </p>
          </div>

          {logoutError ? (
            <p className="text-sm text-destructive" role="alert">
              {logoutError}
            </p>
          ) : null}

          <div className="space-y-2">
            <Button onClick={handleGoToPricing} className="w-full gap-2">
              <CreditCard className="w-4 h-4" />
              View Pricing & Subscribe
            </Button>
            <Button
              variant="outline"
              onClick={handleLogout}
              className="w-full gap-2"
              disabled={isLoggingOut}
            >
              <LogOut className="w-4 h-4" />
              {isLoggingOut ? "Logging Out..." : "Log Out"}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
