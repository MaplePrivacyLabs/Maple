import { useEffect, useRef, useState } from "react";
import { readNativeUserAuth, useOpenSecret } from "@mapleai/sdk";
import { Button } from "@/components/ui/button";
import {
  clearDesktopOAuthTarget,
  isCurrentDesktopOAuthTarget,
  isNativeOAuthRedirect,
  isNativeTargetAllowed,
  mintTransportV2NativeAuthReturn,
  nativeAppLabel,
  type NativeAuthReturn,
  TRANSPORT_V2_PENDING_TTL_MS,
  type TransportV2DesktopOAuthState
} from "@/services/desktopOAuthTransport";

/** Shared by provider redirects and Apple's popup. Identity comes only from authenticated SDK state. */
export function HostedNativeSignInConfirmation({
  target
}: {
  target: TransportV2DesktopOAuthState;
}) {
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
  const [handoff, setHandoff] = useState<NativeAuthReturn | null>(null);
  const appLabel = nativeAppLabel(target);
  const isAgent = target.nativeApp === "agent";

  const ownsAccount = () => {
    if (!active.current || cancelled.current || !account || !isNativeTargetAllowed(target))
      return false;
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
        setHandoff(null);
        setMessage(`This sign-in expired. Start a new login in ${appLabel}.`);
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
  }, [target, appLabel]);

  useEffect(() => {
    if (handoff?.expiresAt === undefined) return;
    const timer = setTimeout(
      () => {
        setHandoff(null);
        setMessage(`This sign-in expired. Start a new login in ${appLabel}.`);
        setStatus("closed");
      },
      Math.max(0, handoff.expiresAt - Date.now())
    );
    return () => clearTimeout(timer);
  }, [handoff, appLabel]);

  const cancel = () => {
    submitted.current = true;
    cancelled.current = true;
    clearDesktopOAuthTarget(target);
    setHandoff(null);
    setMessage(`Sign-in cancelled. You can close this page and return to ${appLabel}.`);
    setStatus("closed");
  };

  const approve = async () => {
    if (submitted.current || !active.current) return;
    submitted.current = true;
    setStatus("minting");
    try {
      const result = await mintTransportV2NativeAuthReturn(
        target,
        os.mintNativeHandoffGrant,
        ownsAccount
      );
      if (!ownsAccount()) return;
      setHandoff(result);
      setStatus("complete");
      // Loopback return must be a separate top-level navigation in a user click.
      if (!isAgent) window.location.href = result.url;
    } catch {
      if (!active.current || cancelled.current) return;
      setMessage(`This sign-in could not be completed. Start a new login in ${appLabel}.`);
      setStatus("closed");
    }
  };

  const openMaple = () => {
    // The target was consumed after minting; a new pending flow invalidates this fallback.
    if (
      !handoff ||
      !ownsAccount() ||
      isNativeOAuthRedirect() ||
      (handoff.expiresAt !== undefined && Date.now() >= handoff.expiresAt)
    ) {
      cancel();
      return;
    }
    window.location.href = handoff.url;
  };

  if (!account) {
    return <p role="alert">Your account could not be verified. Start a new login in {appLabel}.</p>;
  }
  if (status === "closed") return <p role="status">{message}</p>;

  return (
    <div className="space-y-4">
      <div>
        <p>Sign in to {isAgent ? appLabel : "the Maple app"} as</p>
        <p className="font-medium break-all">{account.email || `Account ${account.id}`}</p>
      </div>
      <p className="text-sm text-muted-foreground">
        Continue only if you started this login in {appLabel}.
        {!isAgent && " Check that Maple shows the same account before signing in there."}
      </p>
      <div className="flex flex-wrap justify-end gap-2">
        <Button type="button" variant="outline" onClick={cancel}>
          Cancel
        </Button>
        {status === "complete" ? (
          <Button type="button" onClick={openMaple}>
            {isAgent ? `Return to ${appLabel}` : "Open Maple"}
          </Button>
        ) : (
          <Button
            type="button"
            onClick={approve}
            disabled={status === "minting" || !isCurrentDesktopOAuthTarget(target)}
          >
            {status === "minting" ? "Continuing…" : `Continue to ${appLabel}`}
          </Button>
        )}
      </div>
    </div>
  );
}
