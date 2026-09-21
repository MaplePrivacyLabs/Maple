import { createFileRoute, useNavigate, useRouter, Link } from "@tanstack/react-router";
import { useCallback, useEffect, useState, useRef } from "react";
import { useOpenSecret } from "@mapleai/sdk";
import { HostedNativeSignInConfirmation } from "@/components/HostedNativeSignInConfirmation";
import { AlertDestructive } from "@/components/AlertDestructive";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { getBillingService } from "@/billing/billingService";
import { getSafeInternalRedirect, navigateToSafeInternalRedirect } from "@/utils/internalRedirect";
import {
  clearDesktopOAuthTarget,
  isCurrentDesktopOAuthTarget,
  readTransportV2DesktopOAuth,
  isNativeOAuthRedirect,
  type TransportV2DesktopOAuthState,
  type DesktopOAuthProvider
} from "@/services/desktopOAuthTransport";

export const Route = createFileRoute("/auth/$provider/callback")({
  component: OAuthCallback
});

// Define the utility function within the file
function formatProviderName(provider: string): string {
  switch (provider.toLowerCase()) {
    case "github":
      return "GitHub";
    case "google":
      return "Google";
    case "apple":
      return "Apple";
    default:
      return provider.charAt(0).toUpperCase() + provider.slice(1);
  }
}

function asDesktopOAuthProvider(provider: string): DesktopOAuthProvider | null {
  return provider === "github" || provider === "google" || provider === "apple" ? provider : null;
}

function OAuthCallback() {
  const [isProcessing, setIsProcessing] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [nativeConfirmation, setNativeConfirmation] = useState<TransportV2DesktopOAuthState | null>(
    null
  );
  const active = useRef(true);
  const redirectTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const navigate = useNavigate();
  const router = useRouter();
  const { handleGitHubCallback, handleGoogleCallback } = useOpenSecret();
  const processedRef = useRef(false);

  const { provider } = Route.useParams();
  const formattedProvider = formatProviderName(provider);
  const [nativeFlow] = useState(() => {
    const nativeProvider = asDesktopOAuthProvider(provider);
    return {
      requested: isNativeOAuthRedirect(),
      target: nativeProvider ? readTransportV2DesktopOAuth(nativeProvider) : null
    };
  });

  useEffect(() => {
    active.current = true;
    return () => {
      active.current = false;
      if (redirectTimer.current) clearTimeout(redirectTimer.current);
      queueMicrotask(() => {
        if (!active.current && nativeFlow.target) clearDesktopOAuthTarget(nativeFlow.target);
      });
    };
  }, [nativeFlow]);

  // Helper functions for the callback process
  const handleSuccessfulAuth = useCallback(async () => {
    if (!active.current) return;
    if (nativeFlow.requested) {
      if (!nativeFlow.target || !isCurrentDesktopOAuthTarget(nativeFlow.target)) {
        throw new Error("Native sign-in changed or expired; please restart login in Maple.");
      }
      setNativeConfirmation(nativeFlow.target);
      return;
    }

    // Handle web redirect
    const selectedPlan = sessionStorage.getItem("selected_plan");
    sessionStorage.removeItem("selected_plan");

    const postAuthRedirect = sessionStorage.getItem("post_auth_redirect");
    sessionStorage.removeItem("post_auth_redirect");
    const safePostAuthRedirect = getSafeInternalRedirect(postAuthRedirect);

    redirectTimer.current = setTimeout(() => {
      if (!active.current) return;
      if (selectedPlan) {
        navigate({
          to: "/pricing",
          search: { selected_plan: selectedPlan }
        });
      } else if (safePostAuthRedirect) {
        navigateToSafeInternalRedirect(router.history, safePostAuthRedirect);
      } else {
        navigate({ to: "/" });
      }
    }, 2000);
  }, [nativeFlow, navigate, router]);

  const handleAuthError = useCallback(
    (error: unknown) => {
      if (!active.current) return;
      if (nativeFlow.target) clearDesktopOAuthTarget(nativeFlow.target);
      console.error(`Authentication callback error:`, error);
      if (error instanceof Error) {
        setError(error.message);
      } else {
        setError("Unknown error");
      }
      setIsProcessing(false);
    },
    [nativeFlow]
  );

  useEffect(() => {
    const processCallback = async () => {
      if (processedRef.current) return;
      processedRef.current = true;

      // Browser Apple completion belongs to the popup promise on its initiating page.
      // This static route cannot receive Apple's form_post response.
      if (provider === "apple") {
        handleAuthError(
          new Error(
            "Apple sign-in uses a popup. Return to the sign-in page, allow popups for this site, and try again."
          )
        );
        return;
      }

      // Get URL parameters for redirect-based OAuth providers.
      const urlParams = new URLSearchParams(window.location.search);
      const code = urlParams.get("code");
      const state = urlParams.get("state");

      if (code && state) {
        try {
          // Handle the callback based on the provider
          if (provider === "github") {
            await handleGitHubCallback(code || "", state || "", "");
          } else if (provider === "google") {
            await handleGoogleCallback(code || "", state || "", "");
          } else {
            throw new Error(`Unsupported provider: ${provider}`);
          }

          if (!active.current) return;

          // Clear any existing billing token to prevent session mixing
          try {
            getBillingService().clearToken();
          } catch (billingError) {
            console.warn("Failed to clear billing token:", billingError);
          }

          // Handle the successful authentication (redirect)
          await handleSuccessfulAuth();
        } catch (error) {
          // Handle authentication error
          handleAuthError(error);
        } finally {
          if (active.current) setIsProcessing(false);
        }
      } else {
        if (!active.current) return;
        if (nativeFlow.target) clearDesktopOAuthTarget(nativeFlow.target);
        setError("Invalid callback parameters");
        setIsProcessing(false);
      }
    };

    processCallback();
  }, [
    handleAuthError,
    handleGitHubCallback,
    handleGoogleCallback,
    handleSuccessfulAuth,
    nativeFlow,
    provider
  ]);

  if (nativeConfirmation) {
    return (
      <Card className="max-w-md mx-auto mt-20">
        <CardHeader>
          <CardTitle>Confirm Maple sign-in</CardTitle>
        </CardHeader>
        <CardContent>
          <HostedNativeSignInConfirmation target={nativeConfirmation} />
        </CardContent>
      </Card>
    );
  }

  if (error) {
    const callbackParams = new URLSearchParams(window.location.search);
    const codes = callbackParams.getAll("code");
    const states = callbackParams.getAll("state");
    const showAgentPasteHint =
      (provider === "github" || provider === "google") &&
      !nativeFlow.requested &&
      codes.length === 1 &&
      codes[0].trim().length > 0 &&
      states.length === 1 &&
      states[0].trim().length > 0 &&
      !["error", "error_description", "error_uri"].some((key) => callbackParams.has(key));
    return (
      <Card className="max-w-md mx-auto mt-20">
        <CardHeader>
          <CardTitle>Authentication Failed</CardTitle>
        </CardHeader>
        <CardContent>
          <AlertDestructive title="Error" description={error} />
          {showAgentPasteHint && (
            <details className="mt-4 text-sm text-muted-foreground">
              <summary className="cursor-pointer">Using Maple Agent's paste field?</summary>
              <p className="mt-2">
                If Maple Agent explicitly asked you to paste a callback URL, copy the full address
                from this browser's address bar and paste it only into the sign-in window you
                started. Do not share this address. Otherwise, start a new sign-in.
              </p>
            </details>
          )}
          <div className="mt-4 flex justify-center">
            <Button asChild>
              <Link to="/">Try Again</Link>
            </Button>
          </div>
        </CardContent>
      </Card>
    );
  }

  // If this is a Tauri app auth flow (desktop or mobile), show processing UI
  if (nativeFlow.requested) {
    return (
      <Card className="max-w-md mx-auto mt-20">
        <CardHeader>
          <CardTitle>Processing {formattedProvider} Login</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="mb-4">Completing authentication...</p>
          <div className="flex justify-center">
            <Loader2 className="h-8 w-8 animate-spin" />
          </div>
        </CardContent>
      </Card>
    );
  }

  // Regular processing UI for web flow
  if (isProcessing) {
    return (
      <Card className="max-w-md mx-auto mt-20">
        <CardHeader>
          <CardTitle>Processing {formattedProvider} Login</CardTitle>
        </CardHeader>
        <CardContent className="flex justify-center">
          <Loader2 className="h-8 w-8 animate-spin" />
        </CardContent>
      </Card>
    );
  }

  return (
    <Card className="max-w-md mx-auto mt-20">
      <CardHeader>
        <CardTitle>{formattedProvider} Authentication Successful</CardTitle>
      </CardHeader>
      <CardContent>
        You have successfully authenticated with {formattedProvider}.
        {sessionStorage.getItem("selected_plan")
          ? "Redirecting to complete your plan selection..."
          : "Redirecting to home page..."}
      </CardContent>
    </Card>
  );
}
