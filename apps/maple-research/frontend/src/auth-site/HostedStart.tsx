import { useEffect, useRef, useState } from "react";
import { useOpenSecret } from "@mapleai/sdk";
import {
  claimTransportV2DesktopOAuthInitiation,
  isCurrentDesktopOAuthTarget,
  markTransportV2DesktopOAuth,
  readTransportV2DesktopOAuth,
  type TransportV2DesktopOAuthState
} from "@/services/desktopOAuthTransport";
import { getBrowserOAuthCallbackUrl } from "@/services/oauthConfig";
import type { AuthSiteRoute } from "./route";
import { HostedAppleSignIn } from "./HostedAppleSignIn";

export function HostedStart({ route }: { route: Extract<AuthSiteRoute, { kind: "start" }> }) {
  const os = useOpenSecret();
  const active = useRef(true);
  const started = useRef(false);
  const [target, setTarget] = useState<TransportV2DesktopOAuthState | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    active.current = true;
    if (!started.current) {
      started.current = true;
      void (async () => {
        try {
          const handoffInput = {
            provider: route.provider,
            nativeSessionId: route.nativeSessionId,
            nativeRequestId: route.nativeRequestId
          };
          markTransportV2DesktopOAuth(handoffInput);
          const pending = readTransportV2DesktopOAuth(route.provider);
          if (!pending) throw new Error("Native sign-in is unavailable");
          setTarget(pending);
          if (route.provider === "apple") return;
          if (!claimTransportV2DesktopOAuthInitiation(handoffInput)) {
            setFailed(true);
            return;
          }
          const initiate =
            route.provider === "google" ? os.initiateGoogleAuth : os.initiateGitHubAuth;
          const response = await initiate(
            "",
            getBrowserOAuthCallbackUrl(route.provider, window.location.origin)
          );
          if (!active.current) return;
          if (!isCurrentDesktopOAuthTarget(pending)) {
            setFailed(true);
            return;
          }
          window.location.href = response.auth_url;
        } catch {
          if (active.current) setFailed(true);
        }
      })();
    }
    return () => {
      active.current = false;
    };
  }, [os.initiateGitHubAuth, os.initiateGoogleAuth, route]);

  if (failed) {
    return (
      <p role="alert">This sign-in changed or could not start. Start a new sign-in in Maple.</p>
    );
  }
  if (route.provider === "apple" && target) return <HostedAppleSignIn target={target} />;
  const providerName = { github: "GitHub", google: "Google", apple: "Apple" }[route.provider];
  return <p role="status">Opening {providerName} sign-in…</p>;
}
