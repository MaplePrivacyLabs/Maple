import { useEffect, useRef, useState } from "react";
import { readNativeUserAuth, useOpenSecret } from "@mapleai/sdk";
import { Button } from "@/components/ui/button";
import {
  clearDesktopOAuthTarget,
  isCurrentDesktopOAuthTarget,
  isNativeOAuthRedirect,
  mintTransportV2NativeAuthDeepLink,
  TRANSPORT_V2_PENDING_TTL_MS,
  type TransportV2DesktopOAuthState
} from "@/services/desktopOAuthTransport";

/** Shared by provider redirects and Apple's popup. Identity comes only from authenticated SDK state. */
export function HostedNativeSignInConfirmation({
  target
}: {
  target: TransportV2DesktopOAuthState;
}) {
  const appName = target.nativeAppVariant === "dev" ? "Maple Dev" : "Maple";
  const os = useOpenSecret();
  const currentOs = useRef(os);
  currentOs.current = os;
  const active = useRef(true);
  const submitted = useRef(false);
  const cancelled = useRef(false);
  const [account] = useState(() => {
    const user = os.auth.user?.user;
    let authority;
    try {
      authority = readNativeUserAuth(os.apiUrl);
    } catch {
      return null;
    }
    if (!user?.id || authority.principalId !== user.id || !authority.credentials) return null;
    return { id: user.id, email: user.email, revision: authority.revision, apiUrl: os.apiUrl };
  });
  const [status, setStatus] = useState<"confirm" | "minting" | "complete" | "closed">("confirm");
  const [message, setMessage] = useState<string | null>(null);
  const [deepLink, setDeepLink] = useState<string | null>(null);

  const ownsAccount = () => {
    if (!active.current || cancelled.current || !account) return false;
    const current = currentOs.current;
    if (current.apiUrl !== account.apiUrl || current.auth.user?.user.id !== account.id)
      return false;
    try {
      const authority = readNativeUserAuth(account.apiUrl);
      return authority.principalId === account.id && authority.revision === account.revision;
    } catch {
      return false;
    }
  };

  useEffect(() => {
    active.current = true;
    const timer = setTimeout(
      () => {
        submitted.current = true;
        clearDesktopOAuthTarget(target);
        setDeepLink(null);
        setMessage("This sign-in expired. Start a new login in Maple.");
        setStatus("closed");
      },
      Math.max(0, target.startedAt + TRANSPORT_V2_PENDING_TTL_MS - Date.now())
    );
    return () => {
      active.current = false;
      clearTimeout(timer);
      // StrictMode immediately reconnects this effect. A real unmount owns no late completion.
      queueMicrotask(() => {
        if (!active.current) clearDesktopOAuthTarget(target);
      });
    };
  }, [target]);

  const cancel = () => {
    submitted.current = true;
    cancelled.current = true;
    clearDesktopOAuthTarget(target);
    setDeepLink(null);
    setMessage("Sign-in cancelled. You can close this page and return to Maple.");
    setStatus("closed");
  };

  const approve = async () => {
    if (submitted.current || !active.current) return;
    submitted.current = true;
    setStatus("minting");
    try {
      const url = await mintTransportV2NativeAuthDeepLink(
        target,
        os.mintNativeHandoffGrant,
        ownsAccount
      );
      if (!ownsAccount()) return;
      setDeepLink(url);
      setStatus("complete");
      window.location.href = url;
    } catch {
      if (!active.current || cancelled.current) return;
      setMessage("This sign-in could not be completed. Start a new login in Maple.");
      setStatus("closed");
    }
  };

  const openMaple = () => {
    // The target was consumed after minting; a new pending flow invalidates this fallback.
    if (!deepLink || !ownsAccount() || isNativeOAuthRedirect()) {
      cancel();
      return;
    }
    window.location.href = deepLink;
  };

  if (!account) {
    return <p role="alert">Your account could not be verified. Start a new login in Maple.</p>;
  }
  if (status === "closed") return <p role="status">{message}</p>;

  return (
    <div className="space-y-4">
      <div>
        <p>Sign in to the {appName} app as</p>
        <p className="font-medium break-all">{account.email || `Account ${account.id}`}</p>
      </div>
      <p className="text-sm text-muted-foreground">
        Continue only if you started this login in {appName}. Check that {appName} shows the same
        account before signing in there.
      </p>
      <div className="flex flex-wrap justify-end gap-2">
        <Button type="button" variant="outline" onClick={cancel}>
          Cancel
        </Button>
        {status === "complete" ? (
          <Button type="button" onClick={openMaple}>
            {`Open ${appName}`}
          </Button>
        ) : (
          <Button
            type="button"
            onClick={approve}
            disabled={status === "minting" || !isCurrentDesktopOAuthTarget(target)}
          >
            {status === "minting" ? "Continuing…" : `Continue to ${appName}`}
          </Button>
        )}
      </div>
    </div>
  );
}
