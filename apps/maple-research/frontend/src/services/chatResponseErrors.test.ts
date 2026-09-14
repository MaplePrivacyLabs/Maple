import { describe, expect, test } from "bun:test";
import { APIConnectionError } from "openai";
import {
  isChatRequestDefinitelyNotDispatchedError,
  isChatResponseCancellationAlreadyTerminalError,
  isChatResponseDefinitelyRejectedError,
  isImageDescriptionUnavailableError
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
