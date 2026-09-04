import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useEffect, useRef } from "react";
import { useOpenSecret } from "@mapleai/sdk";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Loader2 } from "lucide-react";
import { AppleAuthProvider } from "@/components/AppleAuthProvider";
import {
  claimTransportV2DesktopOAuthInitiation,
  isCurrentDesktopOAuthTarget,
  readTransportV2DesktopOAuth,
  isTransportV2PublicId,
  markTransportV2DesktopOAuth,
  type TransportV2DesktopOAuthState,
  type DesktopOAuthProvider
} from "@/services/desktopOAuthTransport";

// Define the search parameters interface
interface DesktopAuthSearchParams {
  provider: DesktopOAuthProvider;
  transport: "v2";
  native_session_id: string;
  native_request_id: string;
}

// This route handles OAuth flow for both desktop and mobile Tauri apps
export const Route = createFileRoute("/desktop-auth")({
  component: DesktopAuth,
  validateSearch: (search: Record<string, unknown>): DesktopAuthSearchParams => {
    const provider = search.provider;
    if (provider !== "github" && provider !== "google" && provider !== "apple") {
      throw new Error("Unsupported desktop authentication provider");
    }
    if (search.transport !== "v2") {
      throw new Error("Unsupported desktop authentication transport");
    }
    if (!isTransportV2PublicId(search.native_session_id)) {
      throw new Error("Desktop authentication native session is missing or invalid");
    }
    if (!isTransportV2PublicId(search.native_request_id)) {
      throw new Error("Desktop authentication native request is missing or invalid");
    }
    return {
      provider,
      transport: "v2",
      native_session_id: search.native_session_id,
      native_request_id: search.native_request_id
    };
  }
});

function DesktopAuth() {
  // Use the typed search params
  const search = Route.useSearch();
  const { provider, native_session_id, native_request_id } = search;
  const navigate = useNavigate();
  const os = useOpenSecret();
  const currentOs = useRef(os);
  const active = useRef(true);
  currentOs.current = os;

  useEffect(() => {
    active.current = true;
    const initiateAuth = async () => {
      let target: TransportV2DesktopOAuthState | null = null;
      try {
        const handoffTarget = {
          provider,
          nativeSessionId: native_session_id,
          nativeRequestId: native_request_id
        };
        // These public identifiers address one request already prepared by the
        // native SDK. Keep the exact pair in this browser tab across the
        // provider redirect; no native-local attempt or navigation state is
        // sent to the hosted application.
        markTransportV2DesktopOAuth(handoffTarget);
        target = readTransportV2DesktopOAuth(provider);

        // For Apple, we don't need to do anything here - the AppleAuthProvider
        // component will handle the authentication flow with popup
        if (provider === "apple") {
          return;
        }

        // React StrictMode can reconnect this effect. A
        // particular prepared native request gets one hosted OAuth initiation;
        // retrying starts a fresh request in Maple.
        if (!claimTransportV2DesktopOAuthInitiation(handoffTarget)) {
          return;
        }

        // Initiate appropriate OAuth flow for GitHub and Google
        let auth_url;
        if (provider === "github") {
          const result = await currentOs.current.initiateGitHubAuth("");
          auth_url = result.auth_url;
        } else if (provider === "google") {
          const result = await currentOs.current.initiateGoogleAuth("");
          auth_url = result.auth_url;
        } else {
          throw new Error("Unsupported provider");
        }

        // Late provider initiation cannot redirect a replaced or unmounted flow.
        if (active.current && target && isCurrentDesktopOAuthTarget(target)) {
          window.location.href = auth_url;
        }
      } catch (error) {
        if (!active.current || (target && !isCurrentDesktopOAuthTarget(target))) return;
        console.error(`Failed to initiate ${provider} login:`, error);
        // Redirect to login page on error
        navigate({ to: "/login" });
      }
    };

    initiateAuth();
    return () => {
      active.current = false;
    };
  }, [provider, native_session_id, native_request_id, navigate]);

  // Special handling for Apple OAuth - use popup instead of redirect
  if (provider === "apple") {
    return (
      <Card className="max-w-md mx-auto mt-20">
        <CardHeader>
          <CardTitle>Apple Sign In</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="mb-4">Click the button below to sign in with Apple:</p>
          <AppleAuthProvider
            onError={(error) => {
              console.error("Apple auth error:", error);
              navigate({ to: "/login" });
            }}
            inviteCode=""
          />
        </CardContent>
      </Card>
    );
  }

  return (
    <Card className="max-w-md mx-auto mt-20">
      <CardHeader>
        <CardTitle>Redirecting to {provider}</CardTitle>
      </CardHeader>
      <CardContent>
        <p className="mb-4">Please wait while we redirect you to complete authentication...</p>
        <div className="flex justify-center">
          <Loader2 className="h-8 w-8 animate-spin" />
        </div>
      </CardContent>
    </Card>
  );
}
