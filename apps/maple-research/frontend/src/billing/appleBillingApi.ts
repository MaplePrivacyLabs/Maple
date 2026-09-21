import type { BillingStatus } from "./billingApi";

export interface AppleBillingRequestOptions {
  billingUrl: string;
  token: string;
  signal: AbortSignal;
  fetch?: (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>;
  /** Explicitly enabled only by callers using a local development server. */
  allowInsecureLoopback?: boolean;
}

export interface AppleAccountTokenResponse {
  app_account_token: string;
}

export interface AppleSubscriptionHint {
  provider: string;
  plan: string;
  state: string;
  renews_at: number | null;
  manage: string;
  environment?: string;
}

export type AppleTransactionResponse = Omit<
  BillingStatus,
  "payment_provider" | "product_name" | "subscription_status" | "current_period_end"
> & {
  acknowledged_transaction_id: string;
  payment_provider: BillingStatus["payment_provider"] | "apple";
  // The existing HTTP status contract permits null labels and numeric timestamps.
  product_name: string | null;
  subscription_status: string | null;
  current_period_end: number | string | null;
  ios_iap_enabled?: boolean;
  ios_us_external_link_enabled?: boolean;
  subscriptions?: AppleSubscriptionHint[];
  conflict?: { other_provider: string; action: string } | null;
};

export type AppleBillingErrorCode =
  | "invalid_configuration"
  | "invalid_request"
  | "aborted"
  | "network_error"
  | "invalid_response"
  | "transaction_mismatch"
  | "unauthorized"
  | "forbidden"
  | "invalid_transaction"
  | "conflict"
  | "unavailable"
  | "http_error";

/** Deliberately excludes response bodies, request data, and original errors. */
export class AppleBillingApiError extends Error {
  constructor(
    readonly code: AppleBillingErrorCode,
    readonly status: number | null = null
  ) {
    super(`apple_billing_${code}`);
    this.name = "AppleBillingApiError";
  }
}

const decimalId = /^(?:0|[1-9][0-9]*)$/;
const uuid = /^[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}$/i;

function requestUrl(options: AppleBillingRequestOptions, path: string): string {
  let url: URL;
  try {
    url = new URL(options.billingUrl);
  } catch {
    throw new AppleBillingApiError("invalid_configuration");
  }
  const loopback = ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
  if (
    // Reject non-origin syntax before URL normalization can erase it.
    !/^https?:\/\/[^/?#\\\s@]+\/?$/.test(options.billingUrl) ||
    url.username ||
    url.password ||
    (url.protocol !== "https:" &&
      !(url.protocol === "http:" && loopback && options.allowInsecureLoopback))
  ) {
    throw new AppleBillingApiError("invalid_configuration");
  }
  return new URL(path, url).href;
}

function assertNotAborted(signal: AbortSignal): void {
  if (signal.aborted) throw new AppleBillingApiError("aborted");
}

function httpError(status: number): AppleBillingApiError {
  const code: AppleBillingErrorCode =
    status === 401
      ? "unauthorized"
      : status === 403
        ? "forbidden"
        : status === 400
          ? "invalid_transaction"
          : status === 409
            ? "conflict"
            : status === 429 || status >= 500
              ? "unavailable"
              : "http_error";
  return new AppleBillingApiError(code, status);
}

async function request(
  options: AppleBillingRequestOptions,
  path: string,
  signedTransaction?: string
): Promise<unknown> {
  const url = requestUrl(options, path);
  if (typeof options.token !== "string" || !options.token || /\s/.test(options.token)) {
    throw new AppleBillingApiError("invalid_request");
  }
  assertNotAborted(options.signal);
  let response: Response;
  try {
    response = await (options.fetch ?? globalThis.fetch)(url, {
      method: signedTransaction === undefined ? "GET" : "POST",
      headers: {
        Authorization: `Bearer ${options.token}`,
        Accept: "application/json",
        ...(signedTransaction === undefined ? {} : { "Content-Type": "application/json" })
      },
      ...(signedTransaction === undefined
        ? {}
        : { body: JSON.stringify({ signed_transaction: signedTransaction }) }),
      signal: options.signal,
      credentials: "omit",
      redirect: "error",
      cache: "no-store",
      referrerPolicy: "no-referrer"
    });
  } catch {
    assertNotAborted(options.signal);
    throw new AppleBillingApiError("network_error");
  }
  assertNotAborted(options.signal);
  if (
    response.redirected ||
    response.type === "opaqueredirect" ||
    (response.url && response.url !== url)
  ) {
    throw new AppleBillingApiError("invalid_response");
  }
  if (!response.ok) throw httpError(response.status);
  try {
    const body: unknown = await response.json();
    assertNotAborted(options.signal);
    return body;
  } catch {
    assertNotAborted(options.signal);
    throw new AppleBillingApiError("invalid_response");
  }
}

function invalidResponse(): never {
  throw new AppleBillingApiError("invalid_response");
}

function record(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : invalidResponse();
}

function string(value: unknown): string {
  return typeof value === "string" ? value : invalidResponse();
}

function boolean(value: unknown): boolean {
  return typeof value === "boolean" ? value : invalidResponse();
}

function number(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : invalidResponse();
}

function nullable<T>(value: unknown, parse: (value: unknown) => T): T | null {
  return value === null ? null : parse(value);
}

function subscription(value: unknown): AppleSubscriptionHint {
  const hint = record(value);
  return {
    provider: string(hint.provider),
    plan: string(hint.plan),
    state: string(hint.state),
    renews_at: nullable(hint.renews_at, number),
    manage: string(hint.manage),
    ...(hint.environment === undefined ? {} : { environment: string(hint.environment) })
  };
}

function transactionResponse(body: unknown, expectedId: string): AppleTransactionResponse {
  const value = record(body);
  const acknowledgedId = string(value.acknowledged_transaction_id);
  if (!decimalId.test(acknowledgedId)) invalidResponse();
  if (acknowledgedId !== expectedId) throw new AppleBillingApiError("transaction_mismatch");
  const provider = value.payment_provider;
  if (
    provider !== null &&
    provider !== "apple" &&
    provider !== "stripe" &&
    provider !== "zaprite" &&
    provider !== "subscription_pass"
  ) {
    invalidResponse();
  }
  const result: AppleTransactionResponse = {
    acknowledged_transaction_id: acknowledgedId,
    payment_provider: provider,
    is_subscribed: boolean(value.is_subscribed),
    stripe_customer_id: nullable(value.stripe_customer_id ?? null, string),
    product_id: string(value.product_id),
    product_name: nullable(value.product_name, string),
    subscription_status: nullable(value.subscription_status, string),
    current_period_end: nullable(value.current_period_end, (period) =>
      typeof period === "number" ? number(period) : string(period)
    ),
    can_chat: boolean(value.can_chat),
    chats_remaining: nullable(value.chats_remaining, number),
    total_tokens: nullable(value.total_tokens, number),
    used_tokens: nullable(value.used_tokens, number),
    usage_reset_date: nullable(value.usage_reset_date, string)
  };
  if (value.api_credit_balance !== undefined) {
    result.api_credit_balance = number(value.api_credit_balance);
  }
  if (value.pending_plan_change !== undefined) {
    result.pending_plan_change = nullable(value.pending_plan_change, (change) => {
      const fields = record(change);
      return {
        id: string(fields.id),
        type: string(fields.type),
        target_plan_name: string(fields.target_plan_name),
        status: string(fields.status),
        expires_at: string(fields.expires_at)
      };
    });
  }
  for (const flag of ["ios_iap_enabled", "ios_us_external_link_enabled"] as const) {
    if (value[flag] !== undefined) result[flag] = boolean(value[flag]);
  }
  if (value.subscriptions !== undefined) {
    if (!Array.isArray(value.subscriptions)) invalidResponse();
    result.subscriptions = value.subscriptions.map(subscription);
  }
  if (value.conflict !== undefined) {
    result.conflict = nullable(value.conflict, (conflict) => {
      const fields = record(conflict);
      return { other_provider: string(fields.other_provider), action: string(fields.action) };
    });
  }
  return result;
}

export async function fetchAppleAccountToken(
  options: AppleBillingRequestOptions
): Promise<AppleAccountTokenResponse> {
  const body = record(await request(options, "/v1/maple/subscription/apple/account-token"));
  const token = string(body.app_account_token);
  if (!uuid.test(token)) invalidResponse();
  return { app_account_token: token };
}

export async function submitAppleTransaction(
  options: AppleBillingRequestOptions & {
    signedTransaction: string;
    expectedTransactionId: string;
  }
): Promise<AppleTransactionResponse> {
  if (
    typeof options.signedTransaction !== "string" ||
    !options.signedTransaction.trim() ||
    typeof options.expectedTransactionId !== "string" ||
    !decimalId.test(options.expectedTransactionId)
  ) {
    throw new AppleBillingApiError("invalid_request");
  }
  return transactionResponse(
    await request(options, "/v1/maple/subscription/apple/transactions", options.signedTransaction),
    options.expectedTransactionId
  );
}
