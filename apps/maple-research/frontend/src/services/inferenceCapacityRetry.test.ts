import { describe, expect, test } from "bun:test";
import {
  findOpenSecretInferenceCapacityError,
  OpenSecretInferenceCapacityError
} from "@mapleai/sdk";
import OpenAI from "openai";
import { withInferenceCapacityRetry } from "./inferenceCapacityRetry";

describe("withInferenceCapacityRetry", () => {
  test("performs one initial send and one replay with the same request object", async () => {
    let sends = 0;
    const request = {
      model: "kimi-k3",
      metadata: { internal_message_id: "same" }
    };
    const sentRequests: (typeof request)[] = [];
    const sendLimits: number[] = [];
    const wrapped = new Error("OpenAI wrapper") as Error & { cause?: unknown };
    wrapped.cause = new OpenSecretInferenceCapacityError(503, 0);

    const result = await withInferenceCapacityRetry(async (sendLimit) => {
      sends += 1;
      sendLimits.push(sendLimit);
      sentRequests.push(request);
      if (sends === 1) throw wrapped;
      return "completed";
    }, new AbortController().signal);

    expect(result).toBe("completed");
    expect(sends).toBe(2);
    expect(sentRequests).toEqual([request, request]);
    expect(sentRequests[0]).toBe(sentRequests[1]);
    expect(sendLimits).toEqual([2, 1]);
  });

  test("does not replay generic, structurally spoofed, or over-budget failures", async () => {
    const failures: unknown[] = [
      new Error("generic 503"),
      {
        name: "OpenSecretInferenceCapacityError",
        status: 503,
        retryDelayMs: 0
      },
      new OpenSecretInferenceCapacityError(429, null)
    ];

    for (const failure of failures) {
      let sends = 0;
      await expect(
        withInferenceCapacityRetry(async () => {
          sends += 1;
          throw failure;
        }, new AbortController().signal)
      ).rejects.toBe(failure);
      expect(sends).toBe(1);
    }
  });

  test("aborting during the delay prevents replay", async () => {
    const controller = new AbortController();
    let sends = 0;
    const replay = withInferenceCapacityRetry(async () => {
      sends += 1;
      throw new OpenSecretInferenceCapacityError(503, 60_000);
    }, controller.signal);

    controller.abort();
    await expect(replay).rejects.toMatchObject({ name: "AbortError" });
    expect(sends).toBe(1);
  });

  test("reports a rejected turn before the delay so Stop can restore it and abort replay", async () => {
    const controller = new AbortController();
    let sends = 0;
    let responseMayBeAccepted = false;
    let acknowledgeRejection!: () => void;
    const rejected = new Promise<void>((resolve) => {
      acknowledgeRejection = resolve;
    });
    const replay = withInferenceCapacityRetry(
      async () => {
        sends += 1;
        responseMayBeAccepted = true;
        throw new OpenSecretInferenceCapacityError(503, 60_000);
      },
      controller.signal,
      () => {
        responseMayBeAccepted = false;
        acknowledgeRejection();
      }
    );

    await rejected;
    expect(responseMayBeAccepted).toBe(false);
    controller.abort();
    await expect(replay).rejects.toMatchObject({ name: "AbortError" });
    expect(sends).toBe(1);
  });

  test("a pending Stop or deletion can decline replay after a definite capacity rejection", async () => {
    const capacity = new OpenSecretInferenceCapacityError(503, 0);
    let sends = 0;
    let rejected = 0;
    await expect(
      withInferenceCapacityRetry(
        async () => {
          sends += 1;
          throw capacity;
        },
        new AbortController().signal,
        (error) => {
          rejected += 1;
          throw error;
        }
      )
    ).rejects.toBe(capacity);
    expect(sends).toBe(1);
    expect(rejected).toBe(1);
  });

  test("propagates the sole replay failure after exactly two total sends", async () => {
    let sends = 0;
    const retryFailure = new Error("retry failed");
    await expect(
      withInferenceCapacityRetry(async () => {
        sends += 1;
        if (sends === 1) throw new OpenSecretInferenceCapacityError(429, 0);
        throw retryFailure;
      }, new AbortController().signal)
    ).rejects.toBe(retryFailure);
    expect(sends).toBe(2);
  });

  test("does not replay capacity after SDK repair consumed both send permits", async () => {
    let calls = 0;
    const capacity = new OpenSecretInferenceCapacityError(503, 0, 2);

    await expect(
      withInferenceCapacityRetry(async () => {
        calls += 1;
        throw capacity;
      }, new AbortController().signal)
    ).rejects.toBe(capacity);

    expect(calls).toBe(1);
  });

  test("bounds the real OpenAI wrapper to two transport sends", async () => {
    let sends = 0;
    const openai = new OpenAI({
      apiKey: "not-a-real-api-key",
      baseURL: "https://example.test/v1/",
      dangerouslyAllowBrowser: true,
      fetch: async () => {
        sends += 1;
        throw new OpenSecretInferenceCapacityError(503, 0);
      },
      maxRetries: 0
    });
    const request = { model: "kimi-k3", input: "hello" };

    let error: unknown;
    try {
      await withInferenceCapacityRetry(
        () => openai.responses.create(request),
        new AbortController().signal
      );
    } catch (caught) {
      error = caught;
    }

    expect(sends).toBe(2);
    expect(findOpenSecretInferenceCapacityError(error)).toBeInstanceOf(
      OpenSecretInferenceCapacityError
    );
  });

  test("a pre-aborted signal performs no send", async () => {
    const controller = new AbortController();
    controller.abort();
    let sends = 0;

    await expect(
      withInferenceCapacityRetry(async () => {
        sends += 1;
        return "unexpected";
      }, controller.signal)
    ).rejects.toMatchObject({ name: "AbortError" });
    expect(sends).toBe(0);
  });
});
