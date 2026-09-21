export interface AppleAuthorization {
  code: string;
  state: string;
  id_token?: string;
}

declare global {
  interface Window {
    AppleID?: {
      auth: {
        init: (config: {
          clientId: string;
          scope: string;
          redirectURI: string;
          state: string;
          nonce: string;
          usePopup: boolean;
        }) => void;
        signIn: () => Promise<{ authorization: AppleAuthorization }>;
      };
    };
  }
}

export function getAppleAuthError(value: unknown): Error {
  let error = value instanceof Error ? value : new Error("Apple authentication failed");
  if (!(value instanceof Error) && value && typeof value === "object") {
    const code = (value as Record<string, unknown>).error;
    if (typeof code === "string" && code) error = new Error(code);
  }
  if (error.message === "popup_blocked_by_browser" || error.message === "popup_blocked") {
    return new Error("Allow popups for this site, then select Sign in with Apple again.");
  }
  return error;
}

export function isAppleAuthCancellation(error: Error): boolean {
  return error.message === "user_cancelled_authorize" || error.message === "popup_closed_by_user";
}

export function getAppleAuthorizationNonce(authUrl: string): string {
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
