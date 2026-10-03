import { describe, expect, test } from "bun:test";
import OpenAI, { APIConnectionError } from "openai";
import {
  classifyChatLimitFailure,
  isChatRequestDefinitelyNotDispatchedError,
  isChatResponseCancellationAlreadyTerminalError,
  isChatResponseDefinitelyRejectedError,
  isImageDescriptionUnavailableError,
  type ChatLimitFailure
} from "./chatResponseErrors";

const REQUEST_NOT_DISPATCHED_CODE = "opensecret_request_not_dispatched";

function codedImageDescriptionError(
  status = 503,
  contract = "1",
  code = "image_description_unavailable"
): Error & { status: number; headers: Headers } {
  return Object.assign(new Error("Request failed"), {
    status,
    headers: new Headers({
      "x-opensecret-error-contract": contract,
      "x-opensecret-error-code": code
    })
  });
}

describe("chat response error ownership", () => {
  test("recognizes a nested SDK pre-transport marker without replacing error identity", () => {
    const root = Object.assign(new Error("attestation failed"), {
      requestDispatchCode: REQUEST_NOT_DISPATCHED_CODE,
      definitelyNotDispatched: true
    });

    expect(isChatRequestDefinitelyNotDispatchedError(new Error("wrapped", { cause: root }))).toBe(
      true
    );
  });

  test("recognizes ordinary application rejections", () => {
    expect(isChatResponseDefinitelyRejectedError({ status: 400 })).toBe(true);
    expect(isChatResponseDefinitelyRejectedError({ cause: { status: 422 } })).toBe(true);
    expect(isChatResponseDefinitelyRejectedError({ status: 429 })).toBe(true);
    expect(
      isChatResponseDefinitelyRejectedError({
        status: 503,
        headers: new Headers({
          "x-opensecret-error-contract": "1",
          "x-opensecret-error-code": "image_description_unavailable"
        })
      })
    ).toBe(true);
  });

  test("keeps transport failures, server failures, and request timeouts ambiguous", () => {
    expect(isChatRequestDefinitelyNotDispatchedError(new TypeError("fetch failed"))).toBe(false);
    expect(isChatResponseDefinitelyRejectedError({ status: 500 })).toBe(false);
    expect(
      isChatResponseDefinitelyRejectedError({
        status: 500,
        headers: new Headers({ "x-opensecret-error-contract": "1" })
      })
    ).toBe(false);
    expect(
      isChatResponseDefinitelyRejectedError({
        status: 503,
        headers: new Headers({ "x-opensecret-error-contract": "1" })
      })
    ).toBe(false);
    expect(isChatResponseDefinitelyRejectedError({ status: 408 })).toBe(false);
  });

  test("only recognizes the cancel endpoint's already-terminal response", () => {
    expect(isChatResponseCancellationAlreadyTerminalError({ status: 400 })).toBe(true);
    expect(isChatResponseCancellationAlreadyTerminalError({ cause: { status: 400 } })).toBe(true);
    expect(isChatResponseCancellationAlreadyTerminalError({ status: 503 })).toBe(false);
    expect(isChatResponseCancellationAlreadyTerminalError(new TypeError("fetch failed"))).toBe(
      false
    );
  });
});

const limitCases = [
  { kind: "usage", status: 403, code: "usage_limit_reached", message: "Usage limit reached" },
  {
    kind: "freeToken",
    status: 403,
    code: "free_tier_token_limit_exceeded",
    message: "Free tier token limit exceeded"
  },
  {
    kind: "context",
    status: 413,
    code: "message_exceeds_context_limit",
    message: "Message exceeds context limit"
  }
] as const;

function limitHeaders(code: string, contract = "1"): Headers {
  return new Headers({
    "x-opensecret-error-contract": contract,
    "x-opensecret-error-code": code
  });
}

describe("chat limit classification", () => {
  for (const { kind, status, code, message } of limitCases) {
    const expected = { kind, status, code } as ChatLimitFailure;

    test(`classifies ${kind} from structured metadata without parsing display text`, () => {
      expect(
        classifyChatLimitFailure({
          status,
          headers: limitHeaders(code),
          message: "The server's display text can change"
        })
      ).toEqual(expected);
      expect(classifyChatLimitFailure({ status, code })).toEqual(expected);
      expect(classifyChatLimitFailure({ status, error: { code } })).toEqual(expected);
    });

    test(`finds ${kind} through an OpenAI connection wrapper`, () => {
      const cause = Object.assign(new Error("wrapped server rejection"), {
        status,
        headers: limitHeaders(code),
        code,
        error: { code }
      });
      expect(classifyChatLimitFailure(new APIConnectionError({ cause }))).toEqual(expected);
    });

    test(`retains exact legacy ${kind} errors and wrapped causes`, () => {
      const legacy = Object.assign(
        new Error(`Request failed with status ${status}: ${JSON.stringify({ status, message })}`),
        { status }
      );
      expect(classifyChatLimitFailure(legacy)).toEqual(expected);
      expect(classifyChatLimitFailure(new APIConnectionError({ cause: legacy }))).toEqual(expected);
    });

    for (const additiveEnvelope of [false, true]) {
      test(`pinned OpenAI preserves ${kind} ${additiveEnvelope ? "structured body and" : "legacy body with"} code headers`, async () => {
        let sends = 0;
        const openai = new OpenAI({
          apiKey: "local-test-key",
          baseURL: "https://local-fixture.invalid/v1",
          maxRetries: 0,
          fetch: async () => {
            sends++;
            return Response.json(
              {
                status,
                message,
                ...(additiveEnvelope ? { error: { message, code } } : {})
              },
              { status, headers: limitHeaders(code) }
            );
          }
        });
        let failure: unknown;
        try {
          await openai.responses.create({ model: "local-fixture", input: "fixture", stream: true });
        } catch (error) {
          failure = error;
        }
        expect(failure).toBeInstanceOf(OpenAI.APIError);
        expect(failure).toMatchObject({
          status,
          message: additiveEnvelope ? `${status} ${message}` : `${status} status code (no body)`
        });
        expect(classifyChatLimitFailure(failure)).toEqual(expected);
        expect(sends).toBe(1);
      });
    }
  }

  test("does not classify generic or model-plan denials as exhausted usage", async () => {
    expect(classifyChatLimitFailure({ status: 403 })).toBeNull();
    expect(classifyChatLimitFailure(new Error("403 status code (no body)"))).toBeNull();
    expect(
      classifyChatLimitFailure({
        status: 403,
        headers: limitHeaders("model_not_available_on_plan"),
        code: "model_not_available_on_plan"
      })
    ).toBeNull();
    const client = new OpenAI({
      apiKey: "local-test-key",
      maxRetries: 0,
      fetch: async () =>
        Response.json(
          {
            status: 403,
            message: "Model not available on current plan",
            error: {
              message: "Model not available on current plan",
              code: "model_not_available_on_plan"
            }
          },
          { status: 403, headers: limitHeaders("model_not_available_on_plan") }
        )
    });
    const failure = await client.responses
      .create({ model: "local-fixture", input: "fixture" })
      .catch((error: unknown) => error);
    expect(failure).toMatchObject({ status: 403, code: "model_not_available_on_plan" });
    expect(classifyChatLimitFailure(failure)).toBeNull();
  });

  test("requires the correct status and contract for header codes", () => {
    for (const { status, code } of limitCases) {
      for (const wrongStatus of [undefined, String(status), 400, 401, 408, 429, 500]) {
        expect(
          classifyChatLimitFailure({ status: wrongStatus, headers: limitHeaders(code) })
        ).toBeNull();
      }
      expect(classifyChatLimitFailure({ status, headers: limitHeaders(code, "2") })).toBeNull();
      expect(
        classifyChatLimitFailure({
          status,
          headers: new Headers({ "x-opensecret-error-code": code })
        })
      ).toBeNull();
    }
    expect(classifyChatLimitFailure({ status: 413, code: "usage_limit_reached" })).toBeNull();
    expect(
      classifyChatLimitFailure({ status: 403, code: "message_exceeds_context_limit" })
    ).toBeNull();
  });

  test("rejects unknown, malformed, or conflicting structured metadata without text fallback", () => {
    const legacyMessage =
      'Request failed with status 403: {"status":403,"message":"Usage limit reached"}';
    for (const metadata of [
      { status: 403, code: "unknown", message: legacyMessage },
      { status: 403, code: 403, message: legacyMessage },
      {
        status: 403,
        headers: limitHeaders("usage_limit_reached"),
        code: "model_not_available_on_plan"
      },
      {
        status: 403,
        code: "usage_limit_reached",
        error: { code: "free_tier_token_limit_exceeded" }
      },
      {
        status: 403,
        headers: limitHeaders("usage_limit_reached", "2"),
        code: "usage_limit_reached"
      },
      { status: 403, cause: { code: "usage_limit_reached" } },
      { code: "usage_limit_reached", cause: { status: 403 } },
      { status: 500, cause: { status: 403, code: "usage_limit_reached" } }
    ]) {
      expect(classifyChatLimitFailure(metadata)).toBeNull();
    }
  });

  test("legacy compatibility requires an exact prefix, complete JSON, and matching statuses", () => {
    for (const message of [
      "Usage limit reached",
      'Some other error: Request failed with status 403: {"status":403,"message":"Usage limit reached"}',
      'Request failed with status 403: {"status":413,"message":"Usage limit reached"}',
      'Request failed with status 403: {"status":403,"message":"Model not available on current plan"}',
      'Request failed with status 403: {"status":403,"message":"Usage limit reached soon"}',
      'Request failed with status 403: {"status":"403","message":"Usage limit reached"}',
      'Request failed with status 403: {"status":403,"message":"Usage limit reached"',
      'Request failed with status 403: {"status":403,"message":"Usage limit reached"} trailing',
      'Request failed with status 413: {"status":413,"message":"Usage limit reached"}',
      'Request failed with status 403: {"status":403,"message":"Usage limit reached","error":{"code":"model_not_available_on_plan"}}',
      'Request failed with status 403: {"status":403,"message":"Usage limit reached","padding":"' +
        "x".repeat(4096) +
        '"}'
    ]) {
      expect(classifyChatLimitFailure(new Error(message))).toBeNull();
    }
    expect(
      classifyChatLimitFailure({
        status: 413,
        message: 'Request failed with status 403: {"status":403,"message":"Usage limit reached"}'
      })
    ).toBeNull();
  });

  test("restores a legacy model-plan rejection without treating it as quota", () => {
    const legacy = new Error(
      'Request failed with status 403: {"status":403,"message":"Model not available on current plan"}'
    );
    for (const error of [legacy, new APIConnectionError({ cause: legacy })]) {
      expect(classifyChatLimitFailure(error)).toBeNull();
      expect(isChatResponseDefinitelyRejectedError(error)).toBe(true);
    }
    const context = new Error(
      'Request failed with status 413: {"status":413,"message":"Message exceeds context limit"}'
    );
    expect(isChatResponseDefinitelyRejectedError(context)).toBe(true);
  });

  test("malformed legacy strings do not establish definite response rejection", () => {
    for (const message of [
      'Request failed with status 403: {"status":413,"message":"Usage limit reached"}',
      'Request failed with status 403: {"status":403,"message":"Usage limit reached"',
      'Request failed with status 403: {"status":"403","message":"Usage limit reached"}',
      'Request failed with status 403: {"status":403}',
      'Other message: Request failed with status 403: {"status":403,"message":"Usage limit reached"}',
      'Request failed with status 408: {"status":408,"message":"Usage limit reached"}',
      'Request failed with status 500: {"status":500,"message":"Usage limit reached"}'
    ]) {
      expect(isChatResponseDefinitelyRejectedError(new Error(message))).toBe(false);
    }
  });

  test("cause traversal is bounded and cycle-safe", () => {
    const cycle: { cause?: unknown } = {};
    cycle.cause = cycle;
    expect(classifyChatLimitFailure(cycle)).toBeNull();
    const root = { status: 403, code: "usage_limit_reached" };
    let wrapped: unknown = root;
    for (let depth = 0; depth < 3; depth++) wrapped = { cause: wrapped };
    expect(classifyChatLimitFailure(wrapped)).toMatchObject({ kind: "usage" });
    expect(classifyChatLimitFailure({ cause: wrapped })).toBeNull();
    for (const value of [null, undefined, "403", 403, false]) {
      expect(classifyChatLimitFailure(value)).toBeNull();
    }
  });
});

describe("image-description error classification", () => {
  test("recognizes the coded descriptor failure through the OpenAI connection wrapper", () => {
    const error = new APIConnectionError({ cause: codedImageDescriptionError() });

    expect(isImageDescriptionUnavailableError(error)).toBe(true);
  });

  test("recognizes a top-level OpenAI-style HTTP error", () => {
    expect(isImageDescriptionUnavailableError(codedImageDescriptionError())).toBe(true);
  });

  test("fails closed for unrelated or malformed errors", () => {
    expect(isImageDescriptionUnavailableError(codedImageDescriptionError(500))).toBe(false);
    expect(isImageDescriptionUnavailableError(codedImageDescriptionError(503, "2"))).toBe(false);
    expect(
      isImageDescriptionUnavailableError(codedImageDescriptionError(503, "1", "other_error"))
    ).toBe(false);
    expect(
      isImageDescriptionUnavailableError(
        Object.assign(new Error("missing contract"), {
          status: 503,
          headers: new Headers({
            "x-opensecret-error-code": "image_description_unavailable"
          })
        })
      )
    ).toBe(false);
    expect(isImageDescriptionUnavailableError(new Error("ordinary failure"))).toBe(false);
  });
});
