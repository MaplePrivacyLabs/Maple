/** Only show billing recovery advice for the authenticated backend error contract. */
export function accountDeletionBillingErrorMessage(error: unknown): string | null {
  if (typeof error !== "object" || error === null) return null;
  const metadata = error as { status?: unknown; headers?: unknown };
  if (
    metadata.status !== 503 ||
    !(metadata.headers instanceof Headers) ||
    metadata.headers.get("x-opensecret-error-contract") !== "1" ||
    metadata.headers.get("x-opensecret-error-code") !== "billing_account_deletion_failed"
  ) {
    return null;
  }

  // Do not display an upstream response body, which may contain private data.
  return "Billing cleanup is unavailable. Cancel your subscription manually, then try deleting your account again. Your account has not been deleted.";
}
