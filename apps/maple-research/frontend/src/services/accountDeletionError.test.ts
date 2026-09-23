import { describe, expect, test } from "bun:test";
import { accountDeletionBillingErrorMessage } from "./accountDeletionError";

function backendFailure(status = 503, code = "billing_account_deletion_failed", contract = "1") {
  return Object.assign(new Error("private-provider-error-canary"), {
    status,
    headers: new Headers({
      "x-opensecret-error-contract": contract,
      "x-opensecret-error-code": code
    })
  });
}

describe("account deletion billing errors", () => {
  test("gives manual cancellation and retry advice without exposing the response body", () => {
    const message = accountDeletionBillingErrorMessage(backendFailure());
    expect(message).toBe(
      "Billing cleanup is unavailable. Cancel your subscription manually, then try deleting your account again. Your account has not been deleted."
    );
    expect(message).not.toContain("private-provider-error-canary");
  });

  test("does not confuse a rejected code, generic outage, or unknown contract with billing failure", () => {
    expect(accountDeletionBillingErrorMessage(backendFailure(400))).toBeNull();
    expect(
      accountDeletionBillingErrorMessage(backendFailure(503, "internal_server_error"))
    ).toBeNull();
    expect(
      accountDeletionBillingErrorMessage(
        backendFailure(503, "billing_account_deletion_failed", "2")
      )
    ).toBeNull();
  });

  test("does not infer billing failure from untyped messages or incomplete metadata", () => {
    expect(
      accountDeletionBillingErrorMessage(new Error("billing_account_deletion_failed"))
    ).toBeNull();
    expect(accountDeletionBillingErrorMessage({ status: 503 })).toBeNull();
    expect(accountDeletionBillingErrorMessage("billing_account_deletion_failed")).toBeNull();
    expect(accountDeletionBillingErrorMessage(null)).toBeNull();
  });
});
