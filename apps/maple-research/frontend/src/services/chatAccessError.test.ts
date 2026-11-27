import { describe, expect, test } from "bun:test";
import OpenAI, { APIConnectionError } from "openai";
import { createChatRuntimeStore } from "../contexts/ChatRuntimeContext";
import { recoverDetachedChatComposerDraft, type ChatQueuedMessage } from "./chatComposerQueue";
import { createConversationChatKey } from "./chatRuntimeStore";
import { handleChatAccessError, handleChatStreamLimitFailure } from "./chatAccessError";
import {
  classifyChatLimitFailure,
  classifyChatStreamLimitFailure,
  isChatPlanAccessDeniedError
} from "./chatResponseErrors";

async function serverFailure(status: number, code?: string): Promise<unknown> {
  const message = "Server display text is independent of dialog routing";
  const headers = code
    ? new Headers({
        "x-opensecret-error-contract": "1",
        "x-opensecret-error-code": code
      })
    : new Headers();
  const client = new OpenAI({
    apiKey: "local-test-key",
    baseURL: "https://local-fixture.invalid/v1",
    maxRetries: 0,
    fetch: async () =>
      Response.json(
        { status, message, error: { message, ...(code ? { code } : {}) } },
        {
          status,
          headers
        }
      )
  });
  const error = await client.responses
    .create({ model: "local-fixture", input: "fixture", stream: true })
    .catch((failure: unknown) => failure);
  expect(error).toBeInstanceOf(OpenAI.APIError);
  return error;
}

function runtimeFixture() {
  const store = createChatRuntimeStore<{ id: string }, { id: string }>();
  const key = createConversationChatKey("owner");
  const item: ChatQueuedMessage = {
    queueId: "queue-sent",
    messageId: "message-sent",
    text: "Restore this rejected draft",
    draftImages: [],
    imageUrls: new Map(),
    documentText: "Attached document",
    documentName: "notes.md",
    draftProjectId: "project-owner",
    model: "local-fixture",
    webSearchEnabled: false,
    createdMs: 0
  };
  store.select(key, { messages: [{ id: item.messageId }] });
  store.claimVisibleChat({}, key);
  const run = store.beginRun(key);
  const events: string[] = [];
  const restoreTurn = (message: string) => {
    events.push("restore");
    return store.updateForRun(key, run.token, (snapshot) => ({
      ...snapshot,
      composer: recoverDetachedChatComposerDraft(snapshot.composer, item).composer,
      messages: snapshot.messages.filter((entry) => entry.id !== item.messageId),
      error: message
    }));
  };
  const options = {
    restoreTurn,
    isRunCurrent: () => {
      events.push("current");
      return store.isRunCurrent(key, run.token);
    },
    isRuntimeSelected: () => {
      events.push("selected");
      return store.isChatVisible(key);
    },
    showUpgradeDialog: (feature: "usage" | "tokens") => {
      expect(store.get(key)?.composer.input).toBe(item.text);
      events.push(`upgrade:${feature}`);
    },
    showContextLimitDialog: () => {
      expect(store.get(key)?.composer.input).toBe(item.text);
      events.push("context");
    }
  };
  return { store, key, run, item, events, options };
}

describe("chat access error presentation", () => {
  const routes = [
    {
      productName: "Free",
      status: 403,
      code: "usage_limit_reached",
      message: "You've reached your daily usage limit. Upgrade to Pro for more chats.",
      dialog: "upgrade:usage"
    },
    {
      productName: "Pro",
      status: 403,
      code: "usage_limit_reached",
      message: "You've reached your monthly Pro limit. Upgrade to Max for 10x more usage.",
      dialog: "upgrade:usage"
    },
    {
      productName: "Max",
      status: 403,
      code: "usage_limit_reached",
      message: "You've reached your monthly usage limit. Please wait for the next billing cycle.",
      dialog: "upgrade:usage"
    },
    {
      productName: "Free",
      status: 403,
      code: "free_tier_token_limit_exceeded",
      message:
        "This conversation is too long for the free tier. Upgrade to Pro for longer conversations.",
      dialog: "upgrade:tokens"
    },
    {
      productName: "Pro",
      status: 413,
      code: "message_exceeds_context_limit",
      message: "Your message exceeds the context limit for this model.",
      dialog: "context"
    }
  ];

  for (const { productName, status, code, message, dialog } of routes) {
    test(`${productName} ${code} restores the draft before its dialog`, async () => {
      const fixture = runtimeFixture();
      const error = await serverFailure(status, code);
      expect(handleChatAccessError({ ...fixture.options, error, productName })).toBe(true);
      expect(fixture.store.get(fixture.key)).toMatchObject({
        error: message,
        messages: [],
        composer: {
          input: fixture.item.text,
          documentText: fixture.item.documentText,
          documentName: fixture.item.documentName,
          draftProjectId: fixture.item.draftProjectId
        }
      });
      expect(fixture.events).toEqual(["restore", "current", "selected", dialog]);
      fixture.store.dispose();
    });
  }

  test("structured plan access denial restores without retry advice or an upsell dialog", async () => {
    const fixture = runtimeFixture();
    const error = await serverFailure(403, "model_not_available_on_plan");
    const wrapped = new APIConnectionError({ cause: error as Error });
    expect(isChatPlanAccessDeniedError(wrapped)).toBe(true);
    expect(classifyChatLimitFailure(wrapped)).toBeNull();
    expect(handleChatAccessError({ ...fixture.options, error: wrapped, productName: "Free" })).toBe(
      true
    );
    expect(fixture.store.get(fixture.key)?.error).toBe(
      "This model or feature is not available on your current plan."
    );
    expect(fixture.store.get(fixture.key)?.composer.input).toBe(fixture.item.text);
    expect(fixture.events).toEqual(["restore"]);
    fixture.store.dispose();
  });

  test("generic, unknown, wrong-status, and conflicting denials keep ordinary error handling", async () => {
    const planError = await serverFailure(403, "model_not_available_on_plan");
    for (const error of [
      await serverFailure(403),
      await serverFailure(403, "unknown"),
      await serverFailure(413, "model_not_available_on_plan"),
      Object.assign(planError as Error, { code: "usage_limit_reached" }),
      {
        status: 403,
        code: "model_not_available_on_plan",
        headers: new Headers({ "x-opensecret-error-contract": "2" })
      },
      new Error(
        'Request failed with status 403: {"status":403,"message":"Model not available on current plan"}'
      )
    ]) {
      const fixture = runtimeFixture();
      expect(isChatPlanAccessDeniedError(error)).toBe(false);
      expect(handleChatAccessError({ ...fixture.options, error })).toBe(false);
      expect(fixture.events).toEqual([]);
      expect(fixture.store.get(fixture.key)?.composer.input).toBe("");
      fixture.store.dispose();
    }
  });

  test("an exact legacy quota error still reaches the production dialog handler", () => {
    const fixture = runtimeFixture();
    const error = new APIConnectionError({
      cause: new Error(
        'Request failed with status 403: {"status":403,"message":"Usage limit reached"}'
      )
    });
    expect(handleChatAccessError({ ...fixture.options, error })).toBe(true);
    expect(fixture.events).toEqual(["restore", "current", "selected", "upgrade:usage"]);
    fixture.store.dispose();
  });

  test("failed restoration cannot publish a dialog for a current visible run", async () => {
    const fixture = runtimeFixture();
    expect(
      handleChatAccessError({
        ...fixture.options,
        error: await serverFailure(403, "usage_limit_reached"),
        restoreTurn: () => {
          fixture.events.push("restore-declined");
          return false;
        }
      })
    ).toBe(true);
    expect(fixture.events).toEqual(["restore-declined"]);
    expect(fixture.store.get(fixture.key)?.composer.input).toBe("");
    fixture.store.dispose();
  });

  test("a late failure from a cancelled run cannot restore or publish a dialog", async () => {
    const fixture = runtimeFixture();
    fixture.store.cancelRun(fixture.key, fixture.run.token);
    const replacement = fixture.store.beginRun(fixture.key);
    expect(
      handleChatAccessError({
        ...fixture.options,
        error: await serverFailure(403, "usage_limit_reached")
      })
    ).toBe(true);
    expect(fixture.events).toEqual(["restore"]);
    expect(fixture.store.get(fixture.key)?.composer.input).toBe("");
    expect(fixture.store.get(fixture.key)?.error).toBeNull();
    expect(fixture.store.isRunCurrent(fixture.key, replacement.token)).toBe(true);
    fixture.store.dispose();
  });

  test("cancellation during restoration suppresses the modal after restoring the draft", async () => {
    const fixture = runtimeFixture();
    expect(
      handleChatAccessError({
        ...fixture.options,
        error: await serverFailure(403, "usage_limit_reached"),
        restoreTurn: (message) => {
          const restored = fixture.options.restoreTurn(message);
          fixture.store.cancelRun(fixture.key, fixture.run.token);
          return restored;
        }
      })
    ).toBe(true);
    expect(fixture.events).toEqual(["restore", "current"]);
    expect(fixture.store.get(fixture.key)?.composer.input).toBe(fixture.item.text);
    fixture.store.dispose();
  });

  test("an offscreen failure restores only its owning chat and cannot open a visible modal", async () => {
    const fixture = runtimeFixture();
    const otherKey = createConversationChatKey("visible-other");
    fixture.store.select(otherKey);
    fixture.store.claimVisibleChat({}, otherKey);
    expect(
      handleChatAccessError({
        ...fixture.options,
        error: await serverFailure(413, "message_exceeds_context_limit")
      })
    ).toBe(true);
    expect(fixture.events).toEqual(["restore", "current", "selected"]);
    expect(fixture.store.get(fixture.key)?.composer.input).toBe(fixture.item.text);
    expect(fixture.store.get(otherKey)?.composer.input).toBe("");
    expect(fixture.store.get(otherKey)?.error).toBeNull();
    fixture.store.dispose();
  });

  test("account teardown fences a late plan denial from the disposed runtime", async () => {
    const fixture = runtimeFixture();
    fixture.store.dispose();
    expect(
      handleChatAccessError({
        ...fixture.options,
        error: await serverFailure(403, "model_not_available_on_plan")
      })
    ).toBe(true);
    expect(fixture.events).toEqual(["restore"]);
    expect(fixture.store.get(fixture.key)).toBeUndefined();
  });
});

type StreamFailureFixture = {
  code?: string;
  contract?: { error_contract?: unknown; error_code?: unknown } | null;
};

/** The terminal event of an accepted Responses stream, as the OpenAI client yields it. */
async function streamFailureEvent({ code, contract }: StreamFailureFixture): Promise<unknown> {
  const failed = {
    type: "response.failed",
    response: {
      id: "resp_local_fixture",
      object: "response",
      status: "failed",
      output: [],
      error: { code: code ?? "server_error", message: "Server display text" }
    },
    sequence_number: 2,
    ...(contract === null
      ? {}
      : { opensecret: contract ?? { error_contract: "1", error_code: code } })
  };
  const body = `event: response.failed\ndata: ${JSON.stringify(failed)}\n\n`;
  const client = new OpenAI({
    apiKey: "local-test-key",
    baseURL: "https://local-fixture.invalid/v1",
    maxRetries: 0,
    fetch: async () =>
      new Response(body, { status: 200, headers: { "content-type": "text/event-stream" } })
  });
  const stream = await client.responses.create({
    model: "local-fixture",
    input: "fixture",
    stream: true
  });
  const events: unknown[] = [];
  for await (const event of stream) events.push(event);
  expect(events).toHaveLength(1);
  return events[0];
}

/** Stream limits never restore: the accepted turn is already durable. */
function streamFixture() {
  const fixture = runtimeFixture();
  const notRestored = () => {
    expect(fixture.store.get(fixture.key)?.composer.input).toBe("");
    expect(fixture.store.get(fixture.key)?.messages).toEqual([{ id: fixture.item.messageId }]);
  };
  const options = {
    ...fixture.options,
    showUpgradeDialog: (feature: "usage" | "tokens") => {
      notRestored();
      fixture.events.push(`upgrade:${feature}`);
    },
    showContextLimitDialog: () => {
      notRestored();
      fixture.events.push("context");
    }
  };
  return { ...fixture, options };
}

describe("chat stream limit presentation", () => {
  const routes = [
    {
      productName: "Free",
      code: "free_tier_token_limit_exceeded",
      message:
        "This conversation is too long for the free tier. Upgrade to Pro for longer conversations.",
      dialog: "upgrade:tokens"
    },
    {
      productName: "Free",
      code: "usage_limit_reached",
      message: "You've reached your daily usage limit. Upgrade to Pro for more chats.",
      dialog: "upgrade:usage"
    },
    {
      productName: "Pro",
      code: "message_exceeds_context_limit",
      message: "Your message exceeds the context limit for this model.",
      dialog: "context"
    }
  ];

  for (const { productName, code, message, dialog } of routes) {
    test(`an accepted turn ending in ${code} opens its dialog without restoring the draft`, async () => {
      const fixture = streamFixture();
      const event = await streamFailureEvent({ code });
      expect(handleChatStreamLimitFailure({ ...fixture.options, event, productName })).toBe(
        message
      );
      expect(fixture.events).toEqual(["current", "selected", dialog]);
      expect(fixture.store.get(fixture.key)).toMatchObject({
        messages: [{ id: fixture.item.messageId }],
        composer: { input: "" }
      });
      fixture.store.dispose();
    });
  }

  test("generic, unknown, uncontracted and conflicting stream failures stay generic", async () => {
    for (const event of [
      await streamFailureEvent({}),
      await streamFailureEvent({ code: "server_error" }),
      await streamFailureEvent({ code: "model_not_available_on_plan" }),
      await streamFailureEvent({ code: "free_tier_token_limit_exceeded", contract: null }),
      await streamFailureEvent({
        code: "free_tier_token_limit_exceeded",
        contract: { error_contract: "2", error_code: "free_tier_token_limit_exceeded" }
      }),
      await streamFailureEvent({
        code: "server_error",
        contract: { error_contract: "1", error_code: "free_tier_token_limit_exceeded" }
      }),
      await streamFailureEvent({
        code: "constructor",
        contract: { error_contract: "1", error_code: "constructor" }
      }),
      { type: "error", code: "usage_limit_reached", message: "Usage limit reached" },
      null
    ]) {
      const fixture = streamFixture();
      expect(classifyChatStreamLimitFailure(event)).toBeNull();
      expect(handleChatStreamLimitFailure({ ...fixture.options, event })).toBeNull();
      expect(fixture.events).toEqual([]);
      fixture.store.dispose();
    }
  });

  test("a stream limit from a replaced or offscreen run keeps its message but opens no dialog", async () => {
    const event = await streamFailureEvent({ code: "free_tier_token_limit_exceeded" });

    const replaced = streamFixture();
    replaced.store.cancelRun(replaced.key, replaced.run.token);
    replaced.store.beginRun(replaced.key);
    expect(handleChatStreamLimitFailure({ ...replaced.options, event })).not.toBeNull();
    expect(replaced.events).toEqual(["current"]);
    replaced.store.dispose();

    const offscreen = streamFixture();
    const otherKey = createConversationChatKey("visible-other");
    offscreen.store.select(otherKey);
    offscreen.store.claimVisibleChat({}, otherKey);
    expect(handleChatStreamLimitFailure({ ...offscreen.options, event })).not.toBeNull();
    expect(offscreen.events).toEqual(["current", "selected"]);
    offscreen.store.dispose();
  });
});
