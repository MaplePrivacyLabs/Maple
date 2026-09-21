import React, { useEffect, useRef, useState } from "react";
import { useOpenSecret } from "@mapleai/sdk";
import { Button, type ButtonProps } from "./ui/button";
import { Apple } from "./icons/Apple";
import { HostedNativeSignInConfirmation } from "./HostedNativeSignInConfirmation";
import { getBillingService } from "@/billing/billingService";
import {
  getAppleAuthError,
  getAppleAuthorizationNonce,
  isAppleAuthCancellation,
  type AppleAuthorization
} from "@/services/appleOAuth";
import { getBrowserOAuthCallbackUrl } from "@/services/oauthConfig";
import {
  clearDesktopOAuthTransport,
  clearDesktopOAuthTarget,
  isCurrentDesktopOAuthTarget,
  readTransportV2DesktopOAuth,
  isNativeOAuthRedirect,
  type TransportV2DesktopOAuthState
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

export type { AppleAuthorization } from "@/services/appleOAuth";

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
  const active = useRef(true);
  const ownedTarget = useRef<TransportV2DesktopOAuthState | null>(null);
  const [nativeConfirmation, setNativeConfirmation] = useState<TransportV2DesktopOAuthState | null>(
    null
  );

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

  useEffect(() => {
    active.current = true;
    return () => {
      active.current = false;
      queueMicrotask(() => {
        if (!active.current && ownedTarget.current) clearDesktopOAuthTarget(ownedTarget.current);
      });
    };
  }, []);

  const initializeAppleAuth = async (target: TransportV2DesktopOAuthState | null) => {
    const appleId = window.AppleID;
    if (!appleId) {
      throw new Error("Apple Sign In SDK not loaded");
    }

    if (!isNativeOAuthRedirect()) clearDesktopOAuthTransport();

    // A retry is a new authorization attempt, so it gets a fresh backend state and nonce.
    const redirectURI = getBrowserOAuthCallbackUrl("apple", window.location.origin);
    const initiateResult = await os.initiateAppleAuth(inviteCode || "", redirectURI);
    if (!active.current || (target && !isCurrentDesktopOAuthTarget(target))) return;
    const nonce = getAppleAuthorizationNonce(initiateResult.auth_url);

    const state = initiateResult.state || "";
    sessionStorage.setItem("apple_auth_state", state);

    if (selectedPlan) {
      sessionStorage.setItem("selected_plan", selectedPlan);
    }

    appleId.auth.init({
      clientId: "cloud.opensecret.maple.services",
      scope: "name email",
      redirectURI,
      state,
      nonce,
      usePopup: true
    });
  };

  const completeAuthorization = async (
    authorization: AppleAuthorization,
    nativeFlow: boolean,
    target: TransportV2DesktopOAuthState | null
  ) => {
    sessionStorage.removeItem("apple_auth_state");
    await os.handleAppleCallback(authorization.code, authorization.state, inviteCode || "");

    if (!active.current) return;

    try {
      getBillingService().clearToken();
    } catch (billingError) {
      console.warn("Failed to clear billing token:", billingError);
    }

    if (nativeFlow) {
      if (!target || !isCurrentDesktopOAuthTarget(target)) {
        throw new Error("Native sign-in changed or expired; please restart login in Maple.");
      }
      setNativeConfirmation(target);
      return;
    }

    onSuccess?.();
    redirectAfterLogin?.(selectedPlan);
  };

  const handleAppleSignIn = async () => {
    if (isSignInPending.current || nativeConfirmation || !active.current) return;
    isSignInPending.current = true;
    const nativeFlow = isNativeOAuthRedirect();
    const target = nativeFlow ? readTransportV2DesktopOAuth("apple") : null;
    ownedTarget.current = target;

    try {
      if (nativeFlow && !target) {
        throw new Error("Native sign-in changed or expired; please restart login in Maple.");
      }
      await initializeAppleAuth(target);
      if (!active.current || (target && !isCurrentDesktopOAuthTarget(target))) return;

      // Programmatic Apple sign-in returns one promise that resolves on success and rejects on
      // failure. It is the only completion channel; document events are intentionally unused.
      const appleId = window.AppleID;
      if (!appleId) throw new Error("Apple Sign In SDK not loaded");
      const authResult = await appleId.auth.signIn();
      if (!active.current || (target && !isCurrentDesktopOAuthTarget(target))) return;
      const authorization = authResult?.authorization;
      if (!authorization?.code || !authorization.state) {
        throw new Error("Missing required authentication data");
      }

      await completeAuthorization(authorization, nativeFlow, target);
    } catch (error) {
      if (!active.current || (target && !isCurrentDesktopOAuthTarget(target))) return;
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

  if (nativeConfirmation) {
    return <HostedNativeSignInConfirmation target={nativeConfirmation} />;
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
