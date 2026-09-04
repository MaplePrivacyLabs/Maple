import "../index.css";

import { useEffect, useRef, useState } from "react";
import {
  OpenSecretProvider as LegacyOpenSecretProvider,
  useOpenSecret as useLegacyOpenSecret
} from "@opensecret/react-v1";
import { sha256 } from "@noble/hashes/sha256";
import { bytesToHex } from "@noble/hashes/utils";
import { v4 as uuidv4 } from "uuid";
import { Apple } from "@/components/icons/Apple";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { openSecretClientConfig } from "@/config/openSecretClientConfig";
import {
  clearDesktopOAuthTransport,
  markDesktopOAuthTransport
} from "@/services/desktopOAuthTransport";
import { getSafeInternalRedirect } from "@/utils/internalRedirect";
import { Loader2 } from "lucide-react";

type OAuthProvider = "github" | "google" | "apple";

function readProvider(value: string | null): OAuthProvider | null {
  return value === "github" || value === "google" || value === "apple" ? value : null;
}

function providerFromCallbackPath(pathname: string): OAuthProvider | null {
  return readProvider(pathname.match(/^\/auth\/(github|google|apple)\/callback$/u)?.[1] ?? null);
}

function formatProviderName(provider: OAuthProvider): string {
  if (provider === "github") return "GitHub";
  if (provider === "google") return "Google";
  return "Apple";
}

function clearBillingToken(): void {
  sessionStorage.removeItem("maple_billing_token");
}

function buildLegacyDeepLink(selectedPlan?: string): string {
  const accessToken = localStorage.getItem("access_token")?.trim();
  const refreshToken = localStorage.getItem("refresh_token")?.trim();
  if (!accessToken || !refreshToken) {
    throw new Error("The legacy authentication response did not include both credentials");
  }

  const deepLink = new URL("cloud.opensecret.maple://auth");
  deepLink.searchParams.set("access_token", accessToken);
  deepLink.searchParams.set("refresh_token", refreshToken);

  const postAuthRedirect = getSafeInternalRedirect(sessionStorage.getItem("post_auth_redirect"));
  sessionStorage.removeItem("post_auth_redirect");
  sessionStorage.removeItem("selected_plan");
  if (!selectedPlan && postAuthRedirect) {
    deepLink.searchParams.set("next", postAuthRedirect);
  }

  clearDesktopOAuthTransport();
  return deepLink.toString();
}

function LegacyDesktopOAuthContent() {
  const callbackProvider = providerFromCallbackPath(window.location.pathname);
  if (callbackProvider) {
    return <LegacyOAuthCallback provider={callbackProvider} />;
  }
  return <LegacyOAuthInitiation />;
}

function LegacyOAuthInitiation() {
  const os = useLegacyOpenSecret();
  const search = new URLSearchParams(window.location.search);
  const provider = readProvider(search.get("provider"));
  const selectedPlan = search.get("selected_plan") ?? undefined;
  const next = getSafeInternalRedirect(search.get("next"));
  const [error, setError] = useState<string | null>(null);
  const [nativeRedirectUrl, setNativeRedirectUrl] = useState<string | null>(null);
  const started = useRef(false);

  useEffect(() => {
    if (started.current || !provider) return;
    started.current = true;
    markDesktopOAuthTransport("v1");

    sessionStorage.removeItem("selected_plan");
    if (selectedPlan) sessionStorage.setItem("selected_plan", selectedPlan);
    sessionStorage.removeItem("post_auth_redirect");
    if (next) sessionStorage.setItem("post_auth_redirect", next);

    if (provider === "apple") return;

    const initiate = provider === "github" ? os.initiateGitHubAuth : os.initiateGoogleAuth;
    void initiate("")
      .then(({ auth_url }) => {
        window.location.href = auth_url;
      })
      .catch((cause: unknown) => {
        clearDesktopOAuthTransport();
        console.error(`Failed to initiate legacy ${provider} login:`, cause);
        setError(`Failed to initiate ${formatProviderName(provider)} login`);
      });
  }, [next, os.initiateGitHubAuth, os.initiateGoogleAuth, provider, selectedPlan]);

  const finishAppleAuth = () => {
    try {
      clearBillingToken();
      const deepLinkUrl = buildLegacyDeepLink(selectedPlan);
      setNativeRedirectUrl(deepLinkUrl);
      setTimeout(() => {
        window.location.href = deepLinkUrl;
      }, 1000);
    } catch (cause) {
      clearDesktopOAuthTransport();
      console.error("Failed to finish legacy Apple login:", cause);
      setError(cause instanceof Error ? cause.message : "Failed to finish Apple login");
    }
  };

  if (!provider) {
    return <LegacyError message="Unsupported authentication provider" />;
  }
  if (error) return <LegacyError message={error} />;
  if (nativeRedirectUrl) {
    return <LegacySuccess provider={provider} deepLinkUrl={nativeRedirectUrl} />;
  }

  if (provider === "apple") {
    return (
      <Card className="max-w-md mx-auto mt-20">
        <CardHeader>
          <CardTitle>Apple Sign In</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="mb-4">Click the button below to sign in with Apple:</p>
          <LegacyAppleButton
            onAuthenticated={finishAppleAuth}
            onError={(cause) => {
              clearDesktopOAuthTransport();
              setError(cause.message);
            }}
          />
        </CardContent>
      </Card>
    );
  }

  return <LegacyProcessing provider={provider} />;
}

function LegacyOAuthCallback({ provider }: { provider: OAuthProvider }) {
  const os = useLegacyOpenSecret();
  const [error, setError] = useState<string | null>(null);
  const [nativeRedirectUrl, setNativeRedirectUrl] = useState<string | null>(null);
  const processed = useRef(false);

  useEffect(() => {
    if (processed.current) return;
    processed.current = true;

    const search = new URLSearchParams(window.location.search);
    const code = search.get("code");
    const state = search.get("state");
    if (!code || !state) {
      clearDesktopOAuthTransport();
      setError("Invalid callback parameters");
      return;
    }

    const handle =
      provider === "github"
        ? os.handleGitHubCallback
        : provider === "google"
          ? os.handleGoogleCallback
          : os.handleAppleCallback;

    void handle(code, state, "")
      .then(() => {
        clearBillingToken();
        const selectedPlan = sessionStorage.getItem("selected_plan") ?? undefined;
        const deepLinkUrl = buildLegacyDeepLink(selectedPlan);
        setNativeRedirectUrl(deepLinkUrl);
        setTimeout(() => {
          window.location.href = deepLinkUrl;
        }, 1000);
      })
      .catch((cause: unknown) => {
        clearDesktopOAuthTransport();
        console.error(`Legacy ${provider} authentication callback failed:`, cause);
        setError(cause instanceof Error ? cause.message : "Authentication failed");
      });
  }, [os.handleAppleCallback, os.handleGitHubCallback, os.handleGoogleCallback, provider]);

  if (error) return <LegacyError message={error} />;
  if (nativeRedirectUrl) {
    return <LegacySuccess provider={provider} deepLinkUrl={nativeRedirectUrl} />;
  }
  return <LegacyProcessing provider={provider} />;
}

function LegacyAppleButton({
  onAuthenticated,
  onError
}: {
  onAuthenticated: () => void;
  onError: (error: Error) => void;
}) {
  const os = useLegacyOpenSecret();
  const scriptLoaded = useRef(false);
  const pending = useRef(false);

  useEffect(() => {
    if (scriptLoaded.current) return;
    const script = document.createElement("script");
    script.src =
      "https://appleid.cdn-apple.com/appleauth/static/jsapi/appleid/1/en_US/appleid.auth.js";
    script.async = true;
    document.head.appendChild(script);
    scriptLoaded.current = true;
    return () => {
      script.remove();
      scriptLoaded.current = false;
    };
  }, []);

  const signIn = async () => {
    if (pending.current) return;
    pending.current = true;
    try {
      if (!window.AppleID) throw new Error("Apple Sign In SDK not loaded");
      const initiation = await os.initiateAppleAuth("");
      // The released V1 callback contract expects the original raw nonce in
      // session storage and its SHA-256 digest at Apple. Keep that behavior
      // isolated to this compatibility bundle.
      const rawNonce = uuidv4();
      const hashedNonce = bytesToHex(sha256(new TextEncoder().encode(rawNonce)));
      sessionStorage.setItem("apple_auth_nonce", rawNonce);
      sessionStorage.setItem("apple_auth_state", initiation.state || "");
      window.AppleID.auth.init({
        clientId: "cloud.opensecret.maple.services",
        scope: "name email",
        redirectURI: window.location.origin + "/auth/apple/callback",
        state: initiation.state || "",
        nonce: hashedNonce,
        usePopup: true
      });

      const result = await window.AppleID.auth.signIn();
      if (!result.authorization?.code || !result.authorization.state) {
        throw new Error("Missing required authentication data");
      }
      await os.handleAppleCallback(result.authorization.code, result.authorization.state, "");
      sessionStorage.removeItem("apple_auth_state");
      onAuthenticated();
    } catch (cause) {
      const error = cause instanceof Error ? cause : new Error("Apple authentication failed");
      if (
        error.message !== "user_cancelled_authorize" &&
        error.message !== "popup_closed_by_user"
      ) {
        console.error("Legacy Apple authentication failed:", error);
        onError(error);
      }
    } finally {
      pending.current = false;
    }
  };

  return (
    <Button type="button" onClick={signIn} className="w-full">
      <Apple className="mr-2 h-4 w-4" />
      Log in with Apple
    </Button>
  );
}

function LegacyProcessing({ provider }: { provider: OAuthProvider }) {
  return (
    <Card className="max-w-md mx-auto mt-20">
      <CardHeader>
        <CardTitle>Processing {formatProviderName(provider)} Login</CardTitle>
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

function LegacySuccess({
  provider,
  deepLinkUrl
}: {
  provider: OAuthProvider;
  deepLinkUrl: string;
}) {
  return (
    <Card className="max-w-md mx-auto mt-20">
      <CardHeader>
        <CardTitle>{formatProviderName(provider)} Authentication Successful</CardTitle>
      </CardHeader>
      <CardContent>
        <p className="mb-4">Authentication successful! Tap the button below to return to Maple.</p>
        <div className="flex justify-center">
          <Button onClick={() => (window.location.href = deepLinkUrl)}>Open Maple</Button>
        </div>
      </CardContent>
    </Card>
  );
}

function LegacyError({ message }: { message: string }) {
  return (
    <Card className="max-w-md mx-auto mt-20">
      <CardHeader>
        <CardTitle>Authentication Failed</CardTitle>
      </CardHeader>
      <CardContent>
        <p className="mb-4">{message}</p>
        <Button onClick={() => (window.location.href = "/login")}>Return to login</Button>
      </CardContent>
    </Card>
  );
}

export default function LegacyDesktopOAuthApp() {
  return (
    <LegacyOpenSecretProvider {...openSecretClientConfig()}>
      <LegacyDesktopOAuthContent />
    </LegacyOpenSecretProvider>
  );
}
