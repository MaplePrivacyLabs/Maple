import { getAuthenticatedUserId } from "@mapleai/sdk";

export const CHAT_ACCOUNT_CREDENTIAL_MISMATCH_CODE = "chat_account_credential_mismatch";
const REQUEST_NOT_DISPATCHED_CODE = "opensecret_request_not_dispatched";

type AccountBoundFetch = (input: string | URL | Request, init?: RequestInit) => Promise<Response>;

export class ChatAccountCredentialMismatchError extends Error {
  readonly code = CHAT_ACCOUNT_CREDENTIAL_MISMATCH_CODE;

  constructor() {
    super("The authenticated Chat account changed before this request could start");
    this.name = "ChatAccountCredentialMismatchError";
  }
}

/**
 * Fails a user-scoped operation closed when another tab has replaced the
 * browser credentials since this React tree was rendered.
 */
export function assertChatAccountCredential(
  expectedUserId: string | undefined,
  getPrincipalId: () => string | null = () =>
    getAuthenticatedUserId(import.meta.env.VITE_OPEN_SECRET_API_URL)
): void {
  if (!expectedUserId || getPrincipalId() !== expectedUserId) {
    throw new ChatAccountCredentialMismatchError();
  }
}

/**
 * Binds every Chat network request to the account that created its OpenAI
 * client. Access-token refreshes remain valid because the V2 principal,
 * rather than the token bytes, is compared. Cross-tab account replacement
 * fails before plaintext is handed to the encrypted transport.
 */
export function createAccountBoundChatFetch({
  expectedUserId,
  getPrincipalId,
  fetch
}: {
  expectedUserId: string | undefined;
  getPrincipalId?: () => string | null;
  fetch: AccountBoundFetch;
}): AccountBoundFetch {
  return (input, init) => {
    try {
      assertChatAccountCredential(expectedUserId, getPrincipalId);
    } catch (error) {
      if (typeof error === "object" && error !== null) {
        Object.assign(error, {
          requestDispatchCode: REQUEST_NOT_DISPATCHED_CODE,
          definitelyNotDispatched: true
        });
      }
      return Promise.reject(error);
    }
    return fetch(input, init);
  };
}

export function isChatAccountCredentialMismatchError(error: unknown): boolean {
  let current = error;
  for (let depth = 0; depth < 3 && current && typeof current === "object"; depth += 1) {
    if (
      current instanceof ChatAccountCredentialMismatchError ||
      ("name" in current && current.name === "TransportV2AuthorityChangedError") ||
      ("code" in current && current.code === CHAT_ACCOUNT_CREDENTIAL_MISMATCH_CODE)
    ) {
      return true;
    }
    current = "cause" in current ? current.cause : undefined;
  }
  return false;
}
