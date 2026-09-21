import { useEffect, useRef, useState } from "react";
import { useOpenSecret } from "@mapleai/sdk";
import { HostedNativeSignInConfirmation } from "@/components/HostedNativeSignInConfirmation";
import {
  isCurrentDesktopOAuthTarget,
  isNativeOAuthRedirect,
  readTransportV2DesktopOAuth
} from "@/services/desktopOAuthTransport";
import type { AuthSiteRoute } from "./route";
import { ApplePopupRecovery, CallbackRecovery } from "./CallbackRecovery";

export function HostedCallback({ route }: { route: Extract<AuthSiteRoute, { kind: "callback" }> }) {
  const os = useOpenSecret();
  const active = useRef(true);
  const processed = useRef(false);
  const [target] = useState(() =>
    isNativeOAuthRedirect() ? readTransportV2DesktopOAuth(route.provider) : null
  );
  const [status, setStatus] = useState<"processing" | "confirm" | "failed">("processing");

  useEffect(() => {
    active.current = true;
    if (!processed.current) {
      processed.current = true;
      void (async () => {
        // Browser Apple sign-in completes only in HostedAppleSignIn's popup owner.
        // This registered callback path is a passive recovery page, not a redirect fallback.
        if (route.provider === "apple") {
          setStatus("failed");
          return;
        }
        // A hosted callback must still own its native target. Leave its address untouched.
        if (!target || !isCurrentDesktopOAuthTarget(target)) {
          setStatus("failed");
          return;
        }
        try {
          const callback = {
            github: os.handleGitHubCallback,
            google: os.handleGoogleCallback
          }[route.provider];
          await callback(route.code, route.state, "");
          if (active.current) {
            setStatus(isCurrentDesktopOAuthTarget(target) ? "confirm" : "failed");
          }
        } catch {
          // Do not clear the address or another attempt's pending state on a stale callback.
          // Browser credentials retain the SDK's existing persistence behavior.
          if (active.current) setStatus("failed");
        }
      })();
    }
    return () => {
      active.current = false;
    };
  }, [os.handleGitHubCallback, os.handleGoogleCallback, route, target]);

  if (status === "confirm" && target) return <HostedNativeSignInConfirmation target={target} />;
  if (status === "failed") {
    return (
      <div className="space-y-4">
        {route.provider === "apple" ? (
          <ApplePopupRecovery />
        ) : (
          <>
            <p role="alert">This browser sign-in could not be completed.</p>
            <CallbackRecovery />
          </>
        )}
      </div>
    );
  }
  return <p role="status">Completing sign-in…</p>;
}
