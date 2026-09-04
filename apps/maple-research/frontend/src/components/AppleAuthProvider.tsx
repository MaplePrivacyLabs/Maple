import React, { useEffect, useRef, useState } from "react";
import { useOpenSecret } from "@mapleai/sdk";
import { Button, type ButtonProps } from "./ui/button";
import { Apple } from "./icons/Apple";
import { getBillingService } from "@/billing/billingService";
import {
  clearDesktopOAuthTransport,
  isNativeOAuthRedirect,
  mintTransportV2NativeAuthDeepLink
} from "@/services/desktopOAuthTransport";

interface AppleAuthProviderProps {
  onSuccess?: () => void;
  onError?: (error: Error) => void;
  inviteCode?: string;
  redirectAfterLogin?: (plan?: string) => void;
  selectedPlan?: string;
  className?: string;
  buttonLabel?: string;
  buttonVariant?: ButtonProps["variant"];
  children?: React.ReactNode;
}

export interface AppleAuthorization {
  code: string;
  state: string;
  id_token?: string;
}

declare global {
  interface Window {
    AppleID: {
      auth: {
        init: (config: {
          clientId: string;
          scope: string;
          redirectURI: string;
          state: string;
          nonce: string;
          usePopup: boolean;
        }) => void;
        signIn: () => Promise<{
          authorization: AppleAuthorization;
        }>;
      };
    };
  }
}

function getAppleAuthError(value: unknown): Error {
  if (value instanceof Error) return value;
  if (value && typeof value === "object") {
    const error = (value as Record<string, unknown>).error;
    if (typeof error === "string" && error) return new Error(error);
  }

  return new Error("Apple authentication failed");
}

function isAppleAuthCancellation(error: Error): boolean {
  return error.message === "user_cancelled_authorize" || error.message === "popup_closed_by_user";
}

function getAppleAuthorizationNonce(authUrl: string): string {
  let url: URL;
  try {
    url = new URL(authUrl);
  } catch {
    throw new Error("Apple authorization response did not contain a valid nonce");
  }

  const nonces = url.searchParams.getAll("nonce");
  const nonce = nonces[0];
  if (nonces.length !== 1 || !nonce || !/^[0-9a-f]{64}$/u.test(nonce)) {
    throw new Error("Apple authorization response did not contain a valid nonce");
  }

  return nonce;
}

export function AppleAuthProvider({
  onSuccess,
  onError,
  inviteCode = "",
  redirectAfterLogin,
  selectedPlan,
  className,
  buttonLabel = "Log in with Apple",
  buttonVariant,
  children
}: AppleAuthProviderProps) {
  const os = useOpenSecret();
  const appleScriptLoaded = useRef(false);
  const isSignInPending = useRef(false);
  const nativeRedirectTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [nativeRedirectUrl, setNativeRedirectUrl] = useState<string | null>(null);

  useEffect(() => {
    if (appleScriptLoaded.current) return;
    if (window.location.protocol === "tauri:") return;

    const script = document.createElement("script");
    script.src =
      "https://appleid.cdn-apple.com/appleauth/static/jsapi/appleid/1/en_US/appleid.auth.js";
    script.async = true;
    document.head.appendChild(script);
    appleScriptLoaded.current = true;

    return () => {
      if (script.parentNode) {
        script.parentNode.removeChild(script);
      }
      appleScriptLoaded.current = false;
    };
  }, []);

  useEffect(
    () => () => {
      if (nativeRedirectTimer.current) clearTimeout(nativeRedirectTimer.current);
    },
    []
  );

  const initializeAppleAuth = async () => {
    if (!window.AppleID) {
      throw new Error("Apple Sign In SDK not loaded");
    }

    if (!isNativeOAuthRedirect()) clearDesktopOAuthTransport();

    // A retry is a new authorization attempt, so it gets a fresh backend state and nonce.
    const initiateResult = await os.initiateAppleAuth(inviteCode || "");
    const nonce = getAppleAuthorizationNonce(initiateResult.auth_url);

    const state = initiateResult.state || "";
    sessionStorage.setItem("apple_auth_state", state);

    if (selectedPlan) {
      sessionStorage.setItem("selected_plan", selectedPlan);
    }

    window.AppleID.auth.init({
      clientId: "cloud.opensecret.maple.services",
      scope: "name email",
      redirectURI: window.location.origin + "/auth/apple/callback",
      state,
      nonce,
      usePopup: true
    });
  };

  const completeAuthorization = async (authorization: AppleAuthorization) => {
    sessionStorage.removeItem("apple_auth_state");
    await os.handleAppleCallback(authorization.code, authorization.state, inviteCode || "");

    try {
      getBillingService().clearToken();
    } catch (billingError) {
      console.warn("Failed to clear billing token:", billingError);
    }

    const isTauriAuth = isNativeOAuthRedirect();
    if (isTauriAuth) {
      const deepLinkUrl = await mintTransportV2NativeAuthDeepLink(
        "apple",
        os.mintNativeHandoffGrant
      );

      setNativeRedirectUrl(deepLinkUrl);
      nativeRedirectTimer.current = setTimeout(() => {
        window.location.href = deepLinkUrl;
      }, 1000);
      return;
    }

    onSuccess?.();
    redirectAfterLogin?.(selectedPlan);
  };

  const handleAppleSignIn = async () => {
    if (isSignInPending.current) return;
    isSignInPending.current = true;

    try {
      await initializeAppleAuth();

      // Programmatic Apple sign-in returns one promise that resolves on success and rejects on
      // failure. It is the only completion channel; document events are intentionally unused.
      const authResult = await window.AppleID.auth.signIn();
      const authorization = authResult?.authorization;
      if (!authorization?.code || !authorization.state) {
        throw new Error("Missing required authentication data");
      }

      await completeAuthorization(authorization);
    } catch (error) {
      const signInError = getAppleAuthError(error);
      console.error("[Apple Auth] Sign In failed:", signInError);

      if (!isAppleAuthCancellation(signInError)) {
        onError?.(signInError);
      }
    } finally {
      isSignInPending.current = false;
    }
  };

  if (window.location.protocol === "tauri:") {
    return null;
  }

  if (nativeRedirectUrl) {
    return (
      <Button
        type="button"
        onClick={() => (window.location.href = nativeRedirectUrl)}
        variant={buttonVariant}
        className={className || "w-full"}
      >
        Open Maple
      </Button>
    );
  }

  return children ? (
    <div onClick={handleAppleSignIn} className={className}>
      {children}
    </div>
  ) : (
    <Button
      type="button"
      onClick={handleAppleSignIn}
      variant={buttonVariant}
      className={className || "w-full"}
    >
      <Apple className="mr-2 h-4 w-4" />
      {buttonLabel}
    </Button>
  );
}
