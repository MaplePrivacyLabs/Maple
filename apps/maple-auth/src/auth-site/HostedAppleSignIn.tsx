import { useEffect, useRef, useState } from "react";
import { useOpenSecret } from "@mapleai/sdk";
import { Button } from "@/components/ui/button";
import { HostedNativeSignInConfirmation } from "@/components/HostedNativeSignInConfirmation";
import {
  getAppleAuthorizationNonce,
  getAppleAuthError,
  isAppleAuthCancellation
} from "@/services/appleOAuth";
import { getBrowserOAuthCallbackUrl } from "@/services/oauthConfig";
import {
  isCurrentDesktopOAuthTarget,
  type TransportV2DesktopOAuthState
} from "@/services/desktopOAuthTransport";

function loadApplePopupSdk(): Promise<void> {
  if (window.AppleID) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const script = document.createElement("script");
    script.src =
      "https://appleid.cdn-apple.com/appleauth/static/jsapi/appleid/1/en_US/appleid.auth.js";
    script.async = true;
    script.onload = () => {
      if (window.AppleID) resolve();
      else reject(new Error("Apple sign-in did not load"));
    };
    script.onerror = () => {
      script.remove();
      reject(new Error("Apple sign-in did not load"));
    };
    document.head.appendChild(script);
  });
}

export function HostedAppleSignIn({ target }: { target: TransportV2DesktopOAuthState }) {
  const os = useOpenSecret();
  const currentOs = useRef(os);
  currentOs.current = os;
  const active = useRef(true);
  const preparing = useRef(false);
  const expectedState = useRef<string | null>(null);
  const submitted = useRef(false);
  const [status, setStatus] = useState<"loading" | "ready" | "working" | "retry" | "confirm">(
    "loading"
  );
  const [message, setMessage] = useState<string | null>(null);

  const prepare = async () => {
    if (preparing.current || !active.current) return;
    preparing.current = true;
    submitted.current = false;
    expectedState.current = null;
    setStatus("loading");
    try {
      if (!isCurrentDesktopOAuthTarget(target)) throw new Error("Sign-in expired");
      const redirectURI = getBrowserOAuthCallbackUrl("apple", window.location.origin);
      const [response] = await Promise.all([
        currentOs.current.initiateAppleAuth("", redirectURI),
        loadApplePopupSdk()
      ]);
      if (!active.current) return;
      if (!isCurrentDesktopOAuthTarget(target)) throw new Error("Sign-in changed");
      const apple = window.AppleID;
      if (!apple) throw new Error("Apple sign-in did not load");
      apple.auth.init({
        clientId: "cloud.opensecret.maple.services",
        scope: "name email",
        redirectURI,
        state: response.state,
        nonce: getAppleAuthorizationNonce(response.auth_url),
        usePopup: true
      });
      expectedState.current = response.state;
      setStatus("ready");
    } catch {
      if (active.current) {
        setMessage("Apple sign-in could not start. Try again, or start a new sign-in in Maple.");
        setStatus("retry");
      }
    } finally {
      preparing.current = false;
    }
  };

  useEffect(() => {
    active.current = true;
    void prepare();
    return () => {
      active.current = false;
    };
    // One preparation per mounted native attempt; SDK context updates must not restart it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [target]);

  const signIn = () => {
    if (submitted.current || status !== "ready" || !expectedState.current) return;
    submitted.current = true;
    setMessage(null);
    setStatus("working");
    const state = expectedState.current;
    const failed = (value: unknown) => {
      if (!active.current) return;
      const error = getAppleAuthError(value);
      setMessage(
        isAppleAuthCancellation(error)
          ? "Apple sign-in was cancelled. You can try again."
          : "Apple sign-in could not finish. Allow popups for this site, then try again."
      );
      expectedState.current = null;
      setStatus("retry");
    };
    try {
      if (!isCurrentDesktopOAuthTarget(target)) throw new Error("Sign-in changed");
      // No await before this call: Apple's popup must open in the user's click gesture.
      const apple = window.AppleID;
      if (!apple) throw new Error("Apple sign-in did not load");
      const authorization = apple.auth.signIn();
      void authorization
        .then(async ({ authorization: result }) => {
          if (!active.current) return;
          if (!isCurrentDesktopOAuthTarget(target) || result.state !== state || !result.code) {
            throw new Error("Sign-in changed");
          }
          await currentOs.current.handleAppleCallback(result.code, result.state, "");
          if (!active.current) return;
          if (!isCurrentDesktopOAuthTarget(target)) throw new Error("Sign-in changed");
          setStatus("confirm");
        })
        .catch(failed);
    } catch (error) {
      failed(error);
    }
  };

  if (status === "confirm") return <HostedNativeSignInConfirmation target={target} />;
  return (
    <div className="space-y-4">
      <p>Sign in with Apple to continue to Maple.</p>
      {message && <p role="status">{message}</p>}
      {status === "retry" ? (
        <Button type="button" onClick={() => void prepare()}>
          Try again
        </Button>
      ) : (
        <Button type="button" onClick={signIn} disabled={status !== "ready"}>
          {status === "loading"
            ? "Preparing Apple sign-in…"
            : status === "working"
              ? "Waiting for Apple…"
              : "Sign in with Apple"}
        </Button>
      )}
      <p className="text-sm text-muted-foreground">
        Apple opens in a popup. If you close it or your browser blocks it, you can try again here.
      </p>
    </div>
  );
}
