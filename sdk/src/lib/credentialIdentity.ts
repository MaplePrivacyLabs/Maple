import {
  readTransportV2Credentials,
  snapshotTransportV2Auth,
  clearTransportV2CredentialsIfCurrent,
  TransportV2AuthorityChangedError
} from "./transportV2/auth";

export const ACCOUNT_CREDENTIAL_MISMATCH_CODE = "chat_account_credential_mismatch";

/** The current user identity for this API origin, without exposing credential material. */
export function getAuthenticatedUserId(apiUrl: string): string | null {
  return readTransportV2Credentials(apiUrl, "user")?.principalId ?? null;
}

export function accountCredentialMismatchError(): Error & { code: string } {
  return Object.assign(
    new Error("The authenticated account changed before this request could continue"),
    { name: "AccountCredentialMismatchError", code: ACCOUNT_CREDENTIAL_MISMATCH_CODE }
  );
}

export function assertExpectedAccountPrincipal(expected: string, current: string | null): void {
  if (current !== expected) throw accountCredentialMismatchError();
}

export function isAccountCredentialMismatchError(error: unknown): boolean {
  return (
    error instanceof TransportV2AuthorityChangedError ||
    (typeof error === "object" &&
      error !== null &&
      "code" in error &&
      error.code === ACCOUNT_CREDENTIAL_MISMATCH_CODE)
  );
}

/** Clear this provider's credentials synchronously before best-effort remote logout. */
export function clearUserCredentialsForSignOut(
  apiUrl: string,
  expectedUserId?: string
): string | undefined {
  const credentials = readTransportV2Credentials(apiUrl, "user");
  const snapshot = snapshotTransportV2Auth(apiUrl, "user");
  if (expectedUserId !== undefined && credentials) {
    assertExpectedAccountPrincipal(expectedUserId, credentials.principalId);
  }
  if (
    snapshot.principalId !== (credentials?.principalId ?? null) ||
    (credentials && snapshot.revision !== credentials.revision) ||
    !clearTransportV2CredentialsIfCurrent(snapshot)
  ) {
    throw accountCredentialMismatchError();
  }
  return credentials?.refreshToken;
}

/** Preserve incremental opaque bytes, fencing every read against account replacement. */
export function guardAccountResponse(
  response: Response,
  assertCurrent: () => void,
  reportError: (error: unknown) => unknown = (error) => error
): Response {
  try {
    assertCurrent();
  } catch (error) {
    void response.body?.cancel(error).catch(() => {});
    throw reportError(error);
  }
  if (!response.body) return response;
  const reader = response.body.getReader();
  const body = new ReadableStream<Uint8Array>(
    {
      async pull(controller) {
        try {
          assertCurrent();
          const chunk = await reader.read();
          assertCurrent();
          if (chunk.done) controller.close();
          else controller.enqueue(chunk.value);
        } catch (error) {
          controller.error(reportError(error));
          void reader.cancel(error).catch(() => {});
        }
      },
      cancel(reason) {
        return reader.cancel(reason);
      }
    },
    { highWaterMark: 0 }
  );
  return new Response(body, {
    status: response.status,
    statusText: response.statusText,
    headers: response.headers
  });
}
