import {
  isAgentReturnPort,
  isTransportV2PublicId,
  type DesktopOAuthProvider,
  type NativeOAuthInput
} from "@/services/desktopOAuthTransport";

import {
  authEnvironment,
  isNativeAppVariantAllowed,
  type AuthEnvironment
} from "@/config/authEnvironment";

export type AuthSiteRoute =
  | ({ kind: "start" } & NativeOAuthInput)
  | { kind: "callback"; provider: DesktopOAuthProvider; code: string; state: string }
  | { kind: "complete" }
  | { kind: "invalid" };

const AGENT_START_PARAMETERS = new Set([
  "transport",
  "provider",
  "native_session_id",
  "native_request_id",
  "return_port",
  "return_state"
]);

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
  if (
    location.pathname === "/start" ||
    location.pathname === "/desktop-auth" ||
    location.pathname === "/agent/start"
  ) {
    const agent = location.pathname === "/agent/start";
    const provider = singleParameter(params, "provider");
    const nativeSessionId = singleParameter(params, "native_session_id");
    const nativeRequestId = singleParameter(params, "native_request_id");
    const nativeAppVariant = params.has("native_app_variant")
      ? singleParameter(params, "native_app_variant")
      : undefined;
    if (
      [...params.keys()].some(
        (key) => !(agent ? AGENT_START_PARAMETERS : START_PARAMETERS).has(key)
      ) ||
      singleParameter(params, "transport") !== "v2" ||
      (provider !== "github" && provider !== "google" && provider !== "apple") ||
      !isTransportV2PublicId(nativeSessionId) ||
      !isTransportV2PublicId(nativeRequestId) ||
      (!agent && !isNativeAppVariantAllowed(nativeAppVariant, environment))
    ) {
      return { kind: "invalid" };
    }
    if (agent) {
      const port = singleParameter(params, "return_port");
      const returnState = singleParameter(params, "return_state");
      if (
        !port ||
        !/^[1-9][0-9]{0,4}$/u.test(port) ||
        !isAgentReturnPort(Number(port)) ||
        !isTransportV2PublicId(returnState)
      )
        return { kind: "invalid" };
      return {
        kind: "start",
        provider,
        nativeSessionId,
        nativeRequestId,
        nativeApp: "agent",
        returnPort: Number(port),
        returnState,
        environment
      };
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
