import { isTransportV2PublicId, type DesktopOAuthProvider } from "@/services/desktopOAuthTransport";

import {
  authEnvironment,
  isNativeAppVariantAllowed,
  type AuthEnvironment,
  type NativeAppVariant
} from "@/config/authEnvironment";

export type AuthSiteRoute =
  | {
      kind: "start";
      provider: DesktopOAuthProvider;
      nativeSessionId: string;
      nativeRequestId: string;
      nativeAppVariant?: NativeAppVariant;
    }
  | { kind: "callback"; provider: DesktopOAuthProvider; code: string; state: string }
  | { kind: "complete" }
  | { kind: "invalid" };

const START_PARAMETERS = new Set([
  "transport",
  "provider",
  "native_session_id",
  "native_request_id",
  "native_app_variant"
]);

function singleParameter(params: URLSearchParams, name: string): string | null {
  const values = params.getAll(name);
  return values.length === 1 && values[0].length > 0 ? values[0] : null;
}

export function isAuthCallbackPath(pathname: string): boolean {
  return /^\/auth\/(github|google|apple)\/callback$/u.test(pathname);
}

/** An exact route boundary: this entry never falls through to the main app or V1. */
export function parseAuthSiteRoute(
  location: Pick<Location, "pathname" | "search">,
  environment: AuthEnvironment = authEnvironment()
): AuthSiteRoute {
  const params = new URLSearchParams(location.search);
  if (location.pathname === "/start" || location.pathname === "/desktop-auth") {
    const provider = singleParameter(params, "provider");
    const nativeSessionId = singleParameter(params, "native_session_id");
    const nativeRequestId = singleParameter(params, "native_request_id");
    const nativeAppVariant = params.has("native_app_variant")
      ? singleParameter(params, "native_app_variant")
      : undefined;
    if (
      [...params.keys()].some((key) => !START_PARAMETERS.has(key)) ||
      singleParameter(params, "transport") !== "v2" ||
      (provider !== "github" && provider !== "google" && provider !== "apple") ||
      !isTransportV2PublicId(nativeSessionId) ||
      !isTransportV2PublicId(nativeRequestId) ||
      !isNativeAppVariantAllowed(nativeAppVariant, environment)
    ) {
      return { kind: "invalid" };
    }
    return {
      kind: "start",
      provider,
      nativeSessionId,
      nativeRequestId,
      ...(nativeAppVariant === "dev" ? { nativeAppVariant } : {})
    };
  }
  if (isAuthCallbackPath(location.pathname)) {
    const provider = location.pathname.split("/")[2] as DesktopOAuthProvider;
    const code = singleParameter(params, "code");
    const state = singleParameter(params, "state");
    if (
      !code ||
      !state ||
      params.has("error") ||
      params.has("error_description") ||
      params.has("error_uri")
    ) {
      return { kind: "invalid" };
    }
    // State is opaque. Its authenticated interpretation belongs to the SDK/backend.
    return { kind: "callback", provider, code, state };
  }
  if (location.pathname === "/complete" && !params.size) return { kind: "complete" };
  return { kind: "invalid" };
}
