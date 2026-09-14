import { Fragment, type ReactNode } from "react";
import { ChatRuntimeProvider } from "@/contexts/ChatRuntimeContext";

type RootRuntimeLayoutProps = {
  userId: string | null;
  pathname: string;
  authenticatedHome: ReactNode | null;
  routeContent: ReactNode;
  accountScopedUi: ReactNode;
};

const OAUTH_CALLBACK_PATH = /^\/auth\/([^/]+)\/callback\/?$/;
const DESKTOP_AUTH_PATH = /^\/desktop-auth\/?$/;
const SIGNUP_PATH = /^\/signup\/?$/;

function getAccountScopeKey(userId: string | null): string {
  return userId === null ? "account:signed-out" : `account:user:${userId}`;
}

function getRouteScopeKey(pathname: string, accountScopeKey: string): string {
  const callbackMatch = OAUTH_CALLBACK_PATH.exec(pathname);
  if (callbackMatch) return `route:oauth-callback:${callbackMatch[1]}`;
  // The hosted Apple popup confirms its authenticated account on this route.
  if (DESKTOP_AUTH_PATH.test(pathname)) return "route:desktop-auth";

  // Anonymous signup must retain its component-local Account ID while
  // signUpGuest transitions auth from signed out to the newly created user.
  if (SIGNUP_PATH.test(pathname)) return "route:signup";

  return `route:${accountScopeKey}`;
}

/**
 * Keeps account-scoped chat state around the persistent authenticated home and
 * ordinary routed content. OAuth callback and desktop-auth routes stay outside
 * that provider so authentication preserves their one-shot flow and account
 * confirmation. Signup also retains its route state long enough to show a newly
 * created anonymous user's Account ID. Global account-scoped UI retains its
 * previous remount behavior.
 */
export function RootRuntimeLayout({
  userId,
  pathname,
  authenticatedHome,
  routeContent,
  accountScopedUi
}: RootRuntimeLayoutProps) {
  const accountScopeKey = getAccountScopeKey(userId);
  const routeScopeKey = getRouteScopeKey(pathname, accountScopeKey);
  const isOAuthCallback = OAUTH_CALLBACK_PATH.test(pathname);
  const isDesktopAuth = DESKTOP_AUTH_PATH.test(pathname);
  const isSignup = SIGNUP_PATH.test(pathname);
  const isAuthTransitionRoute = isOAuthCallback || isDesktopAuth || isSignup;
  const keyedRouteContent = <Fragment key={routeScopeKey}>{routeContent}</Fragment>;

  return (
    <>
      <ChatRuntimeProvider key={`chat:${accountScopeKey}`}>
        {authenticatedHome}
        {!isAuthTransitionRoute ? keyedRouteContent : null}
      </ChatRuntimeProvider>
      {isAuthTransitionRoute ? keyedRouteContent : null}
      <Fragment key={`global:${accountScopeKey}`}>{accountScopedUi}</Fragment>
    </>
  );
}
