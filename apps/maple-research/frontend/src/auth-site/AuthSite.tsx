import { useState } from "react";
import { useOpenSecret } from "@mapleai/sdk";
import { HostedStart } from "./HostedStart";
import { HostedCallback } from "./HostedCallback";
import { ApplePopupRecovery, CallbackRecovery } from "./CallbackRecovery";
import { isAuthCallbackPath, parseAuthSiteRoute } from "./route";

export function AuthSite() {
  const { auth } = useOpenSecret();
  const [route] = useState(() => parseAuthSiteRoute(window.location));
  const pendingBootstrap = auth.loading && (route.kind === "start" || route.kind === "callback");
  return (
    <main className="auth-shell">
      <section className="auth-card" aria-labelledby="auth-title">
        <img src="/maple-logo-dark.svg" alt="Maple" width="112" height="48" />
        <h1 id="auth-title" className="text-xl font-semibold">
          {route.kind === "invalid" ? "Sign-in unavailable" : "Sign in to Maple"}
        </h1>
        {pendingBootstrap && <p role="status">Preparing sign-in…</p>}
        {!pendingBootstrap && route.kind === "start" && <HostedStart route={route} />}
        {!pendingBootstrap && route.kind === "callback" && <HostedCallback route={route} />}
        {route.kind === "complete" && (
          <p role="status">You can close this page and return to Maple.</p>
        )}
        {route.kind === "invalid" && (
          <div className="space-y-4">
            <p role="alert">
              This page cannot start a sign-in. Open Maple and start sign-in there.
            </p>
            {window.location.pathname === "/auth/apple/callback" ? (
              <ApplePopupRecovery />
            ) : (
              isAuthCallbackPath(window.location.pathname) && <CallbackRecovery />
            )}
          </div>
        )}
      </section>
    </main>
  );
}
