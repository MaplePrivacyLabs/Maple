import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle
} from "@/components/ui/dialog";
import { useQueryClient } from "@tanstack/react-query";
import { useOpenSecret } from "@mapleai/sdk";
import { useRouter } from "@tanstack/react-router";
import { useState, useEffect } from "react";
import { Loader2, CheckCircle, LogOut } from "lucide-react";
import { Input } from "./ui/input";
import { Label } from "./ui/label";
import { AlertDestructive } from "./AlertDestructive";
import {
  clearMapleApiAuthForUser,
  restoreMapleApiAuthForUser,
  stopAgentRuntimeForUser
} from "@/services/agentRuntimeService";
import { resetWorkspaceModePreference } from "@/services/workspaceModePreference";
import { getBillingService } from "@/billing/billingService";
import { useChatRuntimeStore } from "@/contexts/ChatRuntimeContext";
import { beginAllChatRuntimeDeletionFence } from "@/services/chatRuntimeDeletionFence";
import { assertChatAccountCredential } from "@/services/chatAccountCredential";
import { navigateToSafeInternalRedirect } from "@/utils/internalRedirect";

export function VerificationModal() {
  const os = useOpenSecret();
  const queryClient = useQueryClient();
  const runtimeStore = useChatRuntimeStore<unknown, unknown>();
  const router = useRouter();
  const [isOpen, setIsOpen] = useState(() => {
    if (!os.auth.user) return false;
    // Skip email verification for guest users and in local development
    const isGuestUser = os.auth.user.user.login_method?.toLowerCase() === "guest";
    return !import.meta.env.DEV && !isGuestUser && !os.auth.user.user.email_verified;
  });
  const [isResending, setIsResending] = useState(false);
  const [justResent, setJustResent] = useState(false);
  const [verificationCode, setVerificationCode] = useState("");
  const [isVerifying, setIsVerifying] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [isSigningOut, setIsSigningOut] = useState(false);
  const [signOutError, setSignOutError] = useState<string | null>(null);

  // Update open state based on user verification status
  useEffect(() => {
    if (!os.auth.user) {
      setIsOpen(false);
      // Reset form state when modal closes
      setVerificationCode("");
      setError(null);
      setJustResent(false);
    } else {
      // Skip email verification for guest users and in local development
      const isGuestUser = os.auth.user.user.login_method?.toLowerCase() === "guest";
      const shouldRequireVerification =
        !import.meta.env.DEV && !isGuestUser && !os.auth.user.user.email_verified;
      setIsOpen(shouldRequireVerification);
    }
  }, [os.auth.user]);

  const handleOpenChange = (open: boolean) => {
    // Allow closing if the email is verified, in local development, or guest user
    const isGuestUser = os.auth.user?.user.login_method?.toLowerCase() === "guest";
    if (!open && (os.auth.user?.user.email_verified || import.meta.env.DEV || isGuestUser)) {
      setIsOpen(false);
      // Reset form state when modal closes
      setVerificationCode("");
      setError(null);
      setJustResent(false);
    }
  };

  const handleResendVerification = async () => {
    try {
      setIsResending(true);
      await os.requestNewVerificationEmail();
      setJustResent(true);
      // Reset the "just resent" state after 30 seconds
      setTimeout(() => setJustResent(false), 30000);
    } catch (error) {
      console.error("Failed to resend verification email:", error);
    } finally {
      setIsResending(false);
    }
  };

  const handleVerifyCode = async () => {
    if (!verificationCode.trim()) return;

    try {
      setError(null);
      setIsVerifying(true);
      await os.verifyEmail(verificationCode);
      await os.refetchUser();

      // Check for a pending redirect (e.g. team invite page) after email verification
      const pendingRedirect = sessionStorage.getItem("post_auth_redirect");
      sessionStorage.removeItem("post_auth_redirect");
      navigateToSafeInternalRedirect(router.history, pendingRedirect);
    } catch (err) {
      if (err instanceof Error) {
        setError(err.message);
      } else {
        setError("Failed to verify email. Please check the code and try again.");
      }
    } finally {
      setIsVerifying(false);
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") {
      e.preventDefault();
      handleVerifyCode();
    }
  };

  const handleSignOut = async () => {
    setSignOutError(null);
    setIsSigningOut(true);
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
      setSignOutError("Your account changed in another window. Refresh Maple before signing out.");
      setIsSigningOut(false);
      return;
    }

    try {
      operationBlock = await stopAgentRuntimeForUser(userId);
    } catch (error) {
      console.error("Error stopping Agent Mode:", error);
      releaseChatFence();
      setSignOutError("Maple couldn't stop Agent Mode. Please try logging out again.");
      setIsSigningOut(false);
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
      // Do not carry this account's third-party billing JWT into the next
      // authenticated session in the same WebView.
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
      setSignOutError(
        "Maple couldn't securely reset Agent Mode or finish logging out. Please try again."
      );
    } finally {
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
        setIsSigningOut(false);
      } else {
        operationBlock.retainUntilNextSession();
      }
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={handleOpenChange}>
      <DialogContent className="max-w-[calc(100vw-2rem)] sm:max-w-[425px] [&>button]:hidden">
        <DialogHeader>
          <DialogTitle>Verify Your Email</DialogTitle>
          <DialogDescription>
            Please check your email ({os.auth.user?.user.email}) to verify your account. You'll need
            to verify your email to continue using Maple AI.
          </DialogDescription>
        </DialogHeader>
        <div className="flex flex-col gap-4 py-4">
          <div className="grid gap-2">
            <Label htmlFor="verification-code">Verification Code</Label>
            <div className="flex gap-2">
              <Input
                id="verification-code"
                value={verificationCode}
                onChange={(e) => setVerificationCode(e.target.value)}
                onKeyDown={handleKeyDown}
                placeholder="Enter verification code"
              />
              <Button onClick={handleVerifyCode} disabled={isVerifying || !verificationCode.trim()}>
                {isVerifying ? (
                  <>
                    <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                    Verifying...
                  </>
                ) : (
                  "Verify"
                )}
              </Button>
            </div>
            {error && <AlertDestructive title="Verification Failed" description={error} />}
          </div>
          <div className="flex flex-col gap-2">
            {signOutError ? (
              <AlertDestructive title="Log Out Failed" description={signOutError} />
            ) : null}
            {justResent ? (
              <div className="flex items-center gap-2 text-sm text-muted-foreground bg-muted p-3 rounded-md">
                <CheckCircle className="h-4 w-4 text-maple-success" />
                Verification email sent! Please check your inbox.
              </div>
            ) : (
              <Button onClick={handleResendVerification} disabled={isResending}>
                {isResending ? (
                  <>
                    <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                    Sending...
                  </>
                ) : (
                  "Resend Verification Email"
                )}
              </Button>
            )}
            <Button
              variant="outline"
              onClick={handleSignOut}
              className="gap-2"
              disabled={isSigningOut}
            >
              <LogOut className="w-4 h-4" />
              {isSigningOut ? "Logging Out..." : "Log Out"}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
