import { useCallback, useEffect, useRef, useState } from "react";
import { useOpenSecret } from "@mapleai/sdk";
import { isTauri } from "@/utils/platform";
import { listen } from "@tauri-apps/api/event";
import { getSafeInternalRedirect } from "@/utils/internalRedirect";
import {
  authorizeNativeOAuthCallback,
  isNativeOAuthHandoffGrant,
  redeemNativeOAuthGrant,
  type NativeOAuthAccount
} from "@/services/nativeOAuthAttempt";
import { useNotification } from "@/contexts/NotificationContext";
import { NativeOAuthAccountConfirmation } from "./NativeOAuthAccountConfirmation";

// For direct deep link handling, we'll listen to our custom event
// If we had the types installed, we would use:
// import { onOpenUrl } from '@tauri-apps/plugin-deep-link';

export function DeepLinkHandler({ tauri = isTauri() }: { tauri?: boolean } = {}) {
  const os = useOpenSecret();
  const { showNotification } = useNotification();
  const isAuthenticatedRef = useRef(false);
  const redeemingRef = useRef(false);
  const activeRedemptionRef = useRef<AbortController | null>(null);
  const confirmationRef = useRef<((approved: boolean) => void) | null>(null);
  const [confirmation, setConfirmation] = useState<NativeOAuthAccount | null>(null);
  isAuthenticatedRef.current = Boolean(os.auth.user);

  const resolveConfirmation = useCallback((approved: boolean) => {
    const resolve = confirmationRef.current;
    confirmationRef.current = null;
    setConfirmation(null);
    resolve?.(approved);
  }, []);

  const confirmAccount = useCallback(
    (account: NativeOAuthAccount) =>
      new Promise<boolean>((resolve) => {
        resolveConfirmation(false);
        if (isAuthenticatedRef.current) {
          resolve(false);
          return;
        }
        confirmationRef.current = resolve;
        setConfirmation(account);
      }),
    [resolveConfirmation]
  );

  useEffect(() => {
    if (os.auth.user && confirmationRef.current) {
      activeRedemptionRef.current?.abort();
      resolveConfirmation(false);
    }
  }, [os.auth.user, resolveConfirmation]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;

    const setupDeepLinkHandling = async () => {
      try {
        if (tauri) {
          console.log("[Deep Link] Setting up handler for Tauri app");

          // Listen for the custom event we emit from Rust
          unlisten = await listen<string>("deep-link-received", async (event) => {
            if (disposed) return;
            const url = event.payload;
            let isAuthenticationCallback = false;
            console.log("[Deep Link] Received callback");

            try {
              // Parse the URL to extract parameters
              const urlObj = new URL(url);
              // The URL path structure will be: cloud.opensecret.maple://path?params
              const pathParts = urlObj.pathname.split("/").filter(Boolean);
              const firstPathPart = pathParts[0] || urlObj.hostname;

              // Handle different types of deep links
              if (firstPathPart === "auth") {
                isAuthenticationCallback = true;
                const parameterNames = [...urlObj.searchParams.keys()];
                const grantValues = urlObj.searchParams.getAll("handoff_grant");
                const validEnvelope =
                  urlObj.protocol === "cloud.opensecret.maple:" &&
                  urlObj.hostname === "auth" &&
                  (urlObj.pathname === "" || urlObj.pathname === "/") &&
                  urlObj.username === "" &&
                  urlObj.password === "" &&
                  urlObj.port === "" &&
                  urlObj.hash === "" &&
                  parameterNames.length === 1 &&
                  parameterNames[0] === "handoff_grant" &&
                  grantValues.length === 1 &&
                  isNativeOAuthHandoffGrant(grantValues[0]);
                if (!validEnvelope) {
                  console.error("[Deep Link] Authentication callback is malformed");
                  return;
                }

                const authorization = authorizeNativeOAuthCallback(isAuthenticatedRef.current);
                if (authorization === "already_authenticated") {
                  console.warn("[Deep Link] Ignoring auth callback for an existing session");
                  return;
                }
                if (authorization === "missing_or_expired_attempt") {
                  console.warn("[Deep Link] Ignoring unsolicited or expired auth callback");
                  return;
                }
                if (redeemingRef.current) {
                  console.warn("[Deep Link] Authentication completion is already in progress");
                  return;
                }

                redeemingRef.current = true;
                const controller = new AbortController();
                activeRedemptionRef.current = controller;
                try {
                  const pending = await redeemNativeOAuthGrant(grantValues[0], {
                    confirmAccount,
                    signal: controller.signal
                  });
                  if (!pending || disposed || controller.signal.aborted) return;
                  console.log("[Deep Link] Authentication grant accepted");
                  if (pending.selectedPlan) {
                    window.location.href = `/pricing?selected_plan=${encodeURIComponent(pending.selectedPlan)}`;
                  } else if (pending.next === "/redeem" && pending.redemptionCode) {
                    window.location.href = `/redeem?code=${encodeURIComponent(pending.redemptionCode)}`;
                  } else {
                    window.location.href = getSafeInternalRedirect(pending.next) ?? "/";
                  }
                } finally {
                  if (activeRedemptionRef.current === controller) {
                    activeRedemptionRef.current = null;
                    redeemingRef.current = false;
                    resolveConfirmation(false);
                  }
                }
              } else if (
                firstPathPart === "payment" ||
                firstPathPart === "payment-success" ||
                firstPathPart === "payment-success-credits" ||
                firstPathPart === "payment-canceled" ||
                urlObj.searchParams.has("payment_success") ||
                urlObj.searchParams.has("success") ||
                urlObj.searchParams.has("canceled") ||
                urlObj.searchParams.has("payment_canceled")
              ) {
                // Handle payment deep links from various sources
                const isSuccess =
                  firstPathPart === "payment-success" ||
                  firstPathPart === "payment-success-credits" ||
                  urlObj.searchParams.get("success") === "true" ||
                  urlObj.searchParams.get("payment_success") === "true";

                const isCreditSuccess = firstPathPart === "payment-success-credits";

                const isCanceled =
                  firstPathPart === "payment-canceled" ||
                  urlObj.searchParams.get("canceled") === "true" ||
                  urlObj.searchParams.has("payment_canceled");

                console.log("[Deep Link] Payment callback received:", {
                  isSuccess,
                  isCanceled,
                  path: firstPathPart,
                  source: urlObj.searchParams.get("source")
                });

                // Use window.location instead of navigate
                if (isCreditSuccess) {
                  // Keep the established root callback contract; the home route bridges it into
                  // the dedicated API credits settings page.
                  window.location.href = "/?credits_success=true";
                } else if (isSuccess) {
                  // Navigate to the success page or show a success message
                  window.location.href = "/pricing?success=true";
                } else if (isCanceled) {
                  // Navigate to the canceled page or show a canceled message
                  window.location.href = "/pricing?canceled=true";
                } else {
                  // Handle unknown payment status
                  console.warn("[Deep Link] Unknown payment status in callback");
                  window.location.href = "/pricing";
                }
              } else {
                console.warn("[Deep Link] Unknown deep link type:", firstPathPart);
              }
            } catch (error) {
              if (disposed) return;
              if (isAuthenticationCallback) {
                console.error("[Deep Link] Failed to complete authentication");
                showNotification({
                  type: "error",
                  title: "Sign-in could not be completed",
                  message: "Please restart sign-in from Maple.",
                  duration: 0
                });
              } else {
                console.error("[Deep Link] Failed to process deep link", error);
              }
            }
          });

          if (disposed) {
            unlisten();
            return;
          }

          console.log("[Deep Link] Handler setup complete");
        }
      } catch (error) {
        console.error("[Deep Link] Setup failed:", error);
      }
    };

    setupDeepLinkHandling();

    // Return cleanup function
    return () => {
      disposed = true;
      activeRedemptionRef.current?.abort();
      activeRedemptionRef.current = null;
      redeemingRef.current = false;
      resolveConfirmation(false);
      if (unlisten) unlisten();
    };
  }, [confirmAccount, resolveConfirmation, showNotification, tauri]);

  return confirmation ? (
    <NativeOAuthAccountConfirmation account={confirmation} onDecision={resolveConfirmation} />
  ) : null;
}
