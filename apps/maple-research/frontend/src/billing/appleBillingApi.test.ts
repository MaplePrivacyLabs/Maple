import { expect, mock, spyOn, test } from "bun:test";
import {
  AppleBillingApiError,
  fetchAppleAccountToken,
  submitAppleTransaction,
  type AppleBillingRequestOptions,
  type AppleTransactionResponse
} from "./appleBillingApi";

const billingUrl = "https://billing.example.test";
const accountToken = "11111111-2222-4333-8444-555555555555";
const transactionId = "18446744073709551615";
const signedTransaction = "fake-signed-transaction-canary";
const token = "fake-bearer-token-canary";

function status(): AppleTransactionResponse {
  return {
    acknowledged_transaction_id: transactionId,
    payment_provider: "apple",
    is_subscribed: true,
    stripe_customer_id: null,
    product_id: "pro",
    product_name: "Pro",
    subscription_status: "active",
    current_period_end: "2026-10-01T00:00:00Z",
    can_chat: true,
    chats_remaining: null,
    total_tokens: 100,
    used_tokens: 10,
    usage_reset_date: null
  };
}

function json(value: unknown): Response {
  return Response.json(value);
}

function options(fetch: AppleBillingRequestOptions["fetch"]): AppleBillingRequestOptions {
  return { billingUrl, token, signal: new AbortController().signal, fetch };
}

function submit(
  request: AppleBillingRequestOptions,
  expectedTransactionId = transactionId
): Promise<AppleTransactionResponse> {
  return submitAppleTransaction({ ...request, signedTransaction, expectedTransactionId });
}

async function failure(promise: Promise<unknown>): Promise<AppleBillingApiError> {
  try {
    await promise;
  } catch (error) {
    expect(error).toBeInstanceOf(AppleBillingApiError);
    if (error instanceof AppleBillingApiError) return error;
    throw new Error("Expected a bounded Apple billing error");
  }
  throw new Error("Expected request to fail");
}

test("fetches the account token with isolated bearer authentication and strips unknown fields", async () => {
  const fetch = mock<NonNullable<AppleBillingRequestOptions["fetch"]>>(async () =>
    json({ app_account_token: accountToken, signed_transaction: signedTransaction })
  );
  const request = options(fetch);
  expect(await fetchAppleAccountToken(request)).toEqual({ app_account_token: accountToken });
  expect(fetch).toHaveBeenCalledTimes(1);
  const [url, init] = fetch.mock.calls[0];
  expect(url).toBe(`${billingUrl}/v1/maple/subscription/apple/account-token`);
  expect(init).toEqual({
    method: "GET",
    headers: { Authorization: `Bearer ${token}`, Accept: "application/json" },
    signal: request.signal,
    credentials: "omit",
    redirect: "error",
    cache: "no-store",
    referrerPolicy: "no-referrer"
  });
});

test("posts only the signed transaction and retains a different selected provider and typed hints", async () => {
  const body: AppleTransactionResponse = {
    ...status(),
    payment_provider: "stripe",
    api_credit_balance: 12.5,
    ios_iap_enabled: false,
    ios_us_external_link_enabled: false,
    pending_plan_change: {
      id: "change-1",
      type: "downgrade",
      target_plan_name: "Starter",
      status: "pending",
      expires_at: "2026-10-01T00:00:00Z"
    },
    subscriptions: [
      {
        provider: "apple",
        plan: "Pro",
        state: "active",
        renews_at: 1760000000,
        manage: "app_store",
        environment: "Production"
      },
      { provider: "stripe", plan: "Max", state: "active", renews_at: null, manage: "portal" }
    ],
    conflict: { other_provider: "stripe", action: "cancel_other" }
  };
  const fetch = mock<NonNullable<AppleBillingRequestOptions["fetch"]>>(async () =>
    json({ ...body, signed_transaction: signedTransaction, debug: { token } })
  );
  const request = { ...options(fetch), billingUrl: `${billingUrl}/` };
  expect(await submit(request)).toEqual(body);
  const [url, init] = fetch.mock.calls[0];
  expect(url).toBe(`${billingUrl}/v1/maple/subscription/apple/transactions`);
  expect(init?.method).toBe("POST");
  expect(JSON.parse(init?.body as string)).toEqual({ signed_transaction: signedTransaction });
  expect(init?.headers).toEqual({
    Authorization: `Bearer ${token}`,
    Accept: "application/json",
    "Content-Type": "application/json"
  });
  expect(init?.signal).toBe(request.signal);
  expect(init?.redirect).toBe("error");
  expect(init?.credentials).toBe("omit");
  expect(init?.cache).toBe("no-store");
});

test("accepts exact decimal strings including zero and IDs above Number's precision", async () => {
  for (const id of ["0", transactionId]) {
    for (const provider of ["apple", "stripe", "zaprite", "subscription_pass", null] as const) {
      const body = { ...status(), acknowledged_transaction_id: id, payment_provider: provider };
      expect(
        await submit(
          options(async () => json(body)),
          id
        )
      ).toEqual(body);
    }
  }
});

test("accepts the existing wire status's numeric period, nullable labels, and omitted Stripe ID", async () => {
  const body = {
    ...status(),
    stripe_customer_id: undefined,
    product_name: null,
    subscription_status: null,
    current_period_end: 1760000000
  };
  expect(await submit(options(async () => json(body)))).toEqual({
    ...body,
    stripe_customer_id: null
  });
});

test("rejects noncanonical acknowledgement IDs and exact-ID mismatches", async () => {
  for (const id of [0, 9007199254740992, null, "", "00", "01", "-1", "1e3", " 1", "1\n"]) {
    const error = await failure(
      submit(options(async () => json({ ...status(), acknowledged_transaction_id: id })))
    );
    expect(error.code).toBe("invalid_response");
  }
  const error = await failure(
    submit(options(async () => json({ ...status(), acknowledged_transaction_id: "1" })))
  );
  expect(error.code).toBe("transaction_mismatch");
});

test("rejects malformed billing status instead of accepting an acknowledgement alone", async () => {
  for (const body of [
    null,
    [],
    { acknowledged_transaction_id: transactionId },
    { ...status(), billing_status: status(), can_chat: undefined },
    { ...status(), is_subscribed: "true" },
    { ...status(), stripe_customer_id: 1 },
    { ...status(), product_id: null },
    { ...status(), payment_provider: "unknown" },
    { ...status(), used_tokens: "10" },
    { ...status(), current_period_end: {} }
  ]) {
    expect((await failure(submit(options(async () => json(body))))).code).toBe("invalid_response");
  }
  const nonFiniteJson = JSON.stringify(status()).replace('"used_tokens":10', '"used_tokens":1e999');
  expect((await failure(submit(options(async () => new Response(nonFiniteJson))))).code).toBe(
    "invalid_response"
  );
});

test("validates optional billing fields and nested subscription/conflict hints", async () => {
  for (const extra of [
    { api_credit_balance: "12" },
    { pending_plan_change: {} },
    { ios_iap_enabled: null },
    { ios_us_external_link_enabled: "false" },
    { subscriptions: null },
    { subscriptions: [{}] },
    {
      subscriptions: [
        { provider: "apple", plan: "Pro", state: "active", renews_at: "123", manage: "app_store" }
      ]
    },
    { conflict: [] },
    { conflict: { other_provider: "stripe" } }
  ]) {
    expect((await failure(submit(options(async () => json({ ...status(), ...extra }))))).code).toBe(
      "invalid_response"
    );
  }
  const body = { ...status(), pending_plan_change: null, subscriptions: [], conflict: null };
  expect(await submit(options(async () => json(body)))).toEqual(body);
});

test("rejects malformed account tokens and success envelopes", async () => {
  for (const body of [
    null,
    [],
    {},
    { app_account_token: 123 },
    { app_account_token: "not-a-uuid" },
    { app_account_token: `${accountToken}\n` },
    { app_account_token: `{${accountToken}}` }
  ]) {
    expect((await failure(fetchAppleAccountToken(options(async () => json(body))))).code).toBe(
      "invalid_response"
    );
  }
});

test("rejects unsafe endpoint syntax before sending credentials", async () => {
  const fetch = mock(async () => json({ app_account_token: accountToken }));
  for (const endpoint of [
    "",
    "not-a-url",
    "https://user:password@billing.example.test",
    "https://@billing.example.test",
    "https://billing.example.test/path",
    "https://billing.example.test/..",
    "https://billing.example.test/%2e/",
    "https://billing.example.test?",
    "https://billing.example.test#",
    "https://billing.example.test\\",
    " https://billing.example.test",
    "http://billing.example.test",
    "ftp://billing.example.test",
    "http://127.0.0.1:36552"
  ]) {
    expect(
      (await failure(fetchAppleAccountToken({ ...options(fetch), billingUrl: endpoint }))).code
    ).toBe("invalid_configuration");
  }
  expect(fetch).not.toHaveBeenCalled();
});

test("allows plaintext only for an explicitly selected loopback development origin", async () => {
  for (const endpoint of [
    "http://127.0.0.1:36552",
    "http://localhost:36552",
    "http://[::1]:36552"
  ]) {
    const fetch = mock(async () => json({ app_account_token: accountToken }));
    expect(
      await fetchAppleAccountToken({
        ...options(fetch),
        billingUrl: endpoint,
        allowInsecureLoopback: true
      })
    ).toEqual({ app_account_token: accountToken });
    expect(fetch).toHaveBeenCalledTimes(1);
  }
  const fetch = mock(async () => json({ app_account_token: accountToken }));
  for (const endpoint of [
    "http://billing.example.test",
    "http://localhost.evil.test",
    "http://192.168.1.1"
  ]) {
    expect(
      (
        await failure(
          fetchAppleAccountToken({
            ...options(fetch),
            billingUrl: endpoint,
            allowInsecureLoopback: true
          })
        )
      ).code
    ).toBe("invalid_configuration");
  }
  expect(fetch).not.toHaveBeenCalled();
});

test("does not send invalid caller data or an already-aborted request", async () => {
  const fetch = mock(async () => json(status()));
  for (const invalidToken of ["", "one two", "token\r\nInjected: yes"]) {
    expect((await failure(submit({ ...options(fetch), token: invalidToken }))).code).toBe(
      "invalid_request"
    );
  }
  for (const expectedTransactionId of ["", "-1", "01"]) {
    expect((await failure(submit(options(fetch), expectedTransactionId))).code).toBe(
      "invalid_request"
    );
  }
  expect(
    (
      await failure(
        submitAppleTransaction({
          ...options(fetch),
          signedTransaction: "  ",
          expectedTransactionId: transactionId
        })
      )
    ).code
  ).toBe("invalid_request");
  const controller = new AbortController();
  controller.abort("fake-private-abort-reason");
  expect((await failure(submit({ ...options(fetch), signal: controller.signal }))).code).toBe(
    "aborted"
  );
  expect(fetch).not.toHaveBeenCalled();
});

test("classifies HTTP errors without reading bodies or retrying", async () => {
  for (const [httpStatus, code] of [
    [400, "invalid_transaction"],
    [401, "unauthorized"],
    [403, "forbidden"],
    [409, "conflict"],
    [429, "unavailable"],
    [500, "unavailable"],
    [503, "unavailable"],
    [404, "http_error"],
    [302, "http_error"]
  ] as const) {
    const response = new Response(signedTransaction, { status: httpStatus });
    const readJson = spyOn(response, "json");
    const readText = spyOn(response, "text");
    const fetch = mock(async () => response);
    const error = await failure(submit(options(fetch)));
    expect(error.code).toBe(code);
    expect(error.status).toBe(httpStatus);
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(readJson).not.toHaveBeenCalled();
    expect(readText).not.toHaveBeenCalled();
  }
});

test("rejects a redirected or wrong-origin response even from an injected transport", async () => {
  for (const property of [
    { redirected: true },
    { url: "https://other.example.test/result" },
    { type: "opaqueredirect" }
  ]) {
    const response = json(status());
    Object.defineProperties(
      response,
      Object.fromEntries(Object.entries(property).map(([key, value]) => [key, { value }]))
    );
    expect((await failure(submit(options(async () => response)))).code).toBe("invalid_response");
  }
});

test("cancellation wins when fetch or body parsing completes after disposal", async () => {
  for (const stage of ["fetch", "json", "rejection"] as const) {
    const controller = new AbortController();
    const response = json(status());
    if (stage === "json") {
      spyOn(response, "json").mockImplementation(async () => {
        controller.abort("fake-private-abort-reason");
        return status();
      });
    }
    const fetch = async () => {
      if (stage !== "json") controller.abort("fake-private-abort-reason");
      if (stage === "rejection") throw new Error(signedTransaction);
      return response;
    };
    expect((await failure(submit({ ...options(fetch), signal: controller.signal }))).code).toBe(
      "aborted"
    );
  }
});

test("network and parsing failures expose only bounded errors and emit no logs", async () => {
  const logs = ["log", "warn", "error", "debug", "info"].map((method) =>
    spyOn(console, method as "log").mockImplementation(() => {})
  );
  try {
    const failures = [
      failure(
        submit(
          options(async () => {
            throw new Error(`${token} ${signedTransaction}`);
          })
        )
      ),
      failure(submit(options(async () => new Response(`${signedTransaction} not JSON`)))),
      failure(
        submit(options(async () => json({ ...status(), payment_provider: signedTransaction })))
      )
    ];
    const errors = await Promise.all(failures);
    expect(errors.map((error) => error.code)).toEqual([
      "network_error",
      "invalid_response",
      "invalid_response"
    ]);
    for (const error of errors) {
      expect(error.status).toBeNull();
      expect("cause" in error).toBe(false);
      expect(`${error.message} ${error.stack} ${JSON.stringify(error)}`).not.toContain(token);
      expect(`${error.message} ${error.stack} ${JSON.stringify(error)}`).not.toContain(
        signedTransaction
      );
    }
    for (const log of logs) expect(log).not.toHaveBeenCalled();
  } finally {
    for (const log of logs) log.mockRestore();
  }
});
