import { useEffect, useLayoutEffect, useRef } from "react";
import { useOpenSecret } from "@mapleai/sdk";
import { OpenSecretContextType } from "@mapleai/sdk";
import { createRootRouteWithContext, Outlet, useLocation } from "@tanstack/react-router";
import {
  AuthenticatedHomeContent,
  PersistentHomeNavigationProvider
} from "@/components/AuthenticatedHomeContent";
import { ExternalUrlConfirmHandler } from "@/components/ExternalUrlConfirmHandler";
import { RootRuntimeLayout } from "@/components/RootRuntimeLayout";
import { TeamSeatMismatchAlert } from "@/components/team/TeamSeatMismatchAlert";
import { VerificationModal } from "@/components/VerificationModal";
import { proxyService } from "@/services/proxyService";
import { getSafeInternalRedirect } from "@/utils/internalRedirect";

interface RootRouterContext {
  os: OpenSecretContextType;
}

export type RootSearchParams = {
  login?: string;
  next?: string;
  selected_plan?: string;
  success?: boolean;
  canceled?: boolean;
  provider?: string;
};

export const Route = createRootRouteWithContext<RootRouterContext>()({
  component: Root,
  validateSearch: (search: Record<string, unknown>): RootSearchParams => ({
    login: typeof search.login === "string" ? search.login : undefined,
    next: getSafeInternalRedirect(search.next),
    selected_plan: typeof search.selected_plan === "string" ? search.selected_plan : undefined,
    success: typeof search.success === "boolean" ? search.success : undefined,
    canceled: typeof search.canceled === "boolean" ? search.canceled : undefined,
    provider: typeof search.provider === "string" ? search.provider : undefined
  })
});

function Root() {
  const { auth } = useOpenSecret();
  const userId = auth.user?.user.id || null;
  const location = useLocation();
  const persistentHomeRef = useRef<HTMLDivElement>(null);
  const previousUserIdRef = useRef(userId);

  const isHomeRoute = location.pathname === "/";
  const isSettingsRoute =
    location.pathname === "/settings" || location.pathname.startsWith("/settings/");
  const keepAuthenticatedHomeMounted = !!auth.user && (isHomeRoute || isSettingsRoute);

  useLayoutEffect(() => {
    // Explicit sign-outs reset the local proxy themselves. Also stop and scrub it
    // when the account goes away or changes any other way (session expiry,
    // signing in as someone else) so it never keeps serving the previous key.
    const previousUserId = previousUserIdRef.current;
    previousUserIdRef.current = userId;
    if (!previousUserId || previousUserId === userId) return;
    void proxyService.stopAndResetProxy().catch(() => {});
  }, [userId]);

  useEffect(() => {
    const persistentHome = persistentHomeRef.current;
    if (!persistentHome) return;

    if (isSettingsRoute) {
      persistentHome.setAttribute("inert", "");
    } else {
      persistentHome.removeAttribute("inert");
    }
  }, [isSettingsRoute, keepAuthenticatedHomeMounted]);

  // TODO... put something here, but showing nothing looks nicer than "Loading..."
  if (auth.loading) {
    return <></>;
  }

  return (
    <PersistentHomeNavigationProvider>
      <RootRuntimeLayout
        userId={userId}
        pathname={location.pathname}
        authenticatedHome={
          keepAuthenticatedHomeMounted ? (
            <div
              ref={persistentHomeRef}
              aria-hidden={isSettingsRoute || undefined}
              className={
                isSettingsRoute ? "pointer-events-none fixed inset-0 invisible" : "contents"
              }
            >
              <AuthenticatedHomeContent homeLocationHref={isHomeRoute ? location.href : null} />
            </div>
          ) : null
        }
        routeContent={
          <div
            className={
              isSettingsRoute ? "fixed inset-0 z-50 overflow-hidden bg-background" : "contents"
            }
          >
            <Outlet />
          </div>
        }
        accountScopedUi={
          <>
            {(isHomeRoute || isSettingsRoute) && <VerificationModal />}
            {!isSettingsRoute && <TeamSeatMismatchAlert />}
            <ExternalUrlConfirmHandler />
          </>
        }
      />
    </PersistentHomeNavigationProvider>
  );
}
