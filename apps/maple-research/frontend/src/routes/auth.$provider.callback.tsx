import { createFileRoute, useNavigate, useRouter, Link } from "@tanstack/react-router";
import { useCallback, useEffect, useState, useRef } from "react";
import { useOpenSecret } from "@mapleai/sdk";
import { AlertDestructive } from "@/components/AlertDestructive";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { getBillingService } from "@/billing/billingService";
import { getSafeInternalRedirect, navigateToSafeInternalRedirect } from "@/utils/internalRedirect";
import {
  clearDesktopOAuthTransport,
  isNativeOAuthRedirect,
  mintTransportV2NativeAuthDeepLink,
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
  const [nativeRedirectUrl, setNativeRedirectUrl] = useState<string | null>(null);
  const navigate = useNavigate();
  const router = useRouter();
  const {
    handleGitHubCallback,
    handleGoogleCallback,
    handleAppleCallback,
    mintNativeHandoffGrant
  } = useOpenSecret();
  const processedRef = useRef(false);

  const { provider } = Route.useParams();
  const formattedProvider = formatProviderName(provider);

  // Helper functions for the callback process
  const handleSuccessfulAuth = useCallback(async () => {
    // Check if this is a Tauri app auth flow (desktop or mobile)
    const isTauriAuth = isNativeOAuthRedirect();

    if (isTauriAuth) {
      const nativeProvider = asDesktopOAuthProvider(provider);
      if (!nativeProvider) {
        throw new Error("Desktop authentication state is missing or expired; please restart login");
      }
      const deepLinkUrl = await mintTransportV2NativeAuthDeepLink(
        nativeProvider,
        mintNativeHandoffGrant
      );

      // Store the URL in state so we can show a manual open button as fallback
      setNativeRedirectUrl(deepLinkUrl);

      // Try auto-redirect (may be blocked by iOS Safari without user gesture)
      setTimeout(() => {
        window.location.href = deepLinkUrl;
      }, 1000);

      return;
    }

    // Handle web redirect
    const selectedPlan = sessionStorage.getItem("selected_plan");
    sessionStorage.removeItem("selected_plan");

    const postAuthRedirect = sessionStorage.getItem("post_auth_redirect");
    sessionStorage.removeItem("post_auth_redirect");
    const safePostAuthRedirect = getSafeInternalRedirect(postAuthRedirect);

    setTimeout(() => {
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
  }, [mintNativeHandoffGrant, navigate, provider, router]);

  const handleAuthError = (error: unknown) => {
    if (isNativeOAuthRedirect()) clearDesktopOAuthTransport();
    console.error(`Authentication callback error:`, error);
    if (error instanceof Error) {
      setError(error.message);
    } else {
      setError("Unknown error");
    }
    setIsProcessing(false);
  };

  useEffect(() => {
    const processCallback = async () => {
      if (processedRef.current) return;
      processedRef.current = true;

      // Get URL parameters for all OAuth providers
      const urlParams = new URLSearchParams(window.location.search);
      const code = urlParams.get("code");
      const state = urlParams.get("state");

      // For Apple, we might get form data instead of URL parameters
      // Apple uses form_post with POST request in some scenarios
      let appleData = null;
      if (provider === "apple" && !code) {
        // Check if we have Apple data in sessionStorage from form_post
        const appleFormData = sessionStorage.getItem("apple_form_data");
        if (appleFormData) {
          try {
            appleData = JSON.parse(appleFormData);
            sessionStorage.removeItem("apple_form_data");
          } catch (e) {
            console.error("Failed to parse Apple form data:", e);
          }
        }
      }

      if ((code && state) || (provider === "apple" && appleData)) {
        try {
          // Handle the callback based on the provider
          if (provider === "github") {
            await handleGitHubCallback(code || "", state || "", "");
          } else if (provider === "google") {
            await handleGoogleCallback(code || "", state || "", "");
          } else if (provider === "apple") {
            // This handles the redirect flow (backup for non-popup scenarios)
            // Most Apple auth will now be handled client-side in the AppleAuthProvider component
            await handleAppleCallback(code || "", state || "", "");
          } else {
            throw new Error(`Unsupported provider: ${provider}`);
          }

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
          setIsProcessing(false);
        }
      } else {
        if (isNativeOAuthRedirect()) clearDesktopOAuthTransport();
        setError("Invalid callback parameters");
        setIsProcessing(false);
      }
    };

    processCallback();
  }, [
    handleAppleCallback,
    handleGitHubCallback,
    handleGoogleCallback,
    handleSuccessfulAuth,
    provider
  ]);

  // After auth completes for a native app flow, show a button to open the app
  if (nativeRedirectUrl) {
    return (
      <Card className="max-w-md mx-auto mt-20">
        <CardHeader>
          <CardTitle>{formattedProvider} Authentication Successful</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="mb-4">
            Authentication successful! Tap the button below to return to Maple.
          </p>
          <div className="flex justify-center">
            <Button onClick={() => (window.location.href = nativeRedirectUrl)}>Open Maple</Button>
          </div>
        </CardContent>
      </Card>
    );
  }

  if (error) {
    return (
      <Card className="max-w-md mx-auto mt-20">
        <CardHeader>
          <CardTitle>Authentication Failed</CardTitle>
        </CardHeader>
        <CardContent>
          <AlertDestructive title="Error" description={error} />
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
  if (isNativeOAuthRedirect()) {
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
