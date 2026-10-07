import { expect, test } from "bun:test";
import {
  fetchLogin,
  fetchSignUp,
  transcribeAudio,
  deleteConversations,
  batchDeleteConversations,
  listConversations,
  createApiKey,
  deleteApiKey,
  systemOne,
  SystemOneError,
  type SystemOneRequest,
  type SystemOneResponse
} from "../../api";
import { createCustomFetch } from "../../ai";
import OpenAI from "openai";

const TEST_EMAIL = process.env.VITE_TEST_EMAIL;
const TEST_PASSWORD = process.env.VITE_TEST_PASSWORD;
const TEST_CLIENT_ID = process.env.VITE_TEST_CLIENT_ID;
const API_URL = process.env.VITE_OPEN_SECRET_API_URL;
const CHAT_MODEL = process.env.VITE_TEST_CHAT_MODEL ?? "llama3-3-70b";
const TTS_MODEL = process.env.VITE_TEST_TTS_MODEL ?? "qwen3-tts";

if (!TEST_EMAIL || !TEST_PASSWORD || !TEST_CLIENT_ID || !API_URL) {
  throw new Error("Test credentials must be set in .env.local");
}

type ChatMessage = {
  role: "user" | "assistant";
  content: string;
};

async function setupTestUser() {
  try {
    const { access_token, refresh_token } = await fetchLogin(
      TEST_EMAIL!,
      TEST_PASSWORD!,
      TEST_CLIENT_ID!
    );
    window.localStorage.setItem("access_token", access_token);
    window.localStorage.setItem("refresh_token", refresh_token);
  } catch (error) {
    console.log("Login failed, attempting signup");
    await fetchSignUp(TEST_EMAIL!, TEST_PASSWORD!, "", TEST_CLIENT_ID!, "Test User");
    const { access_token, refresh_token } = await fetchLogin(
      TEST_EMAIL!,
      TEST_PASSWORD!,
      TEST_CLIENT_ID!
    );
    window.localStorage.setItem("access_token", access_token);
    window.localStorage.setItem("refresh_token", refresh_token);
  }
}

function collectErrorText(error: unknown, seen = new Set<unknown>(), depth = 0): string {
  if (error === null || error === undefined || depth > 4) {
    return "";
  }
  if (typeof error === "string" || typeof error === "number" || typeof error === "boolean") {
    return String(error);
  }
  if (typeof error !== "object") {
    return "";
  }
  if (seen.has(error)) {
    return "";
  }
  seen.add(error);

  const parts: string[] = [];
  if (error instanceof Error) {
    parts.push(error.name, error.message);
    parts.push(collectErrorText((error as Error & { cause?: unknown }).cause, seen, depth + 1));
  }

  const record = error as Record<string, unknown>;
  for (const key of ["message", "status", "statusText", "error", "cause", "response", "body"]) {
    if (key in record) {
      parts.push(collectErrorText(record[key], seen, depth + 1));
    }
  }

  return parts.filter(Boolean).join("\n");
}

let liveAiUsageLimitSeen = false;

function isLiveAiUnavailableError(error: unknown): boolean {
  const errorText = collectErrorText(error);
  if (/Usage limit reached|insufficient_quota/i.test(errorText)) {
    liveAiUsageLimitSeen = true;
    return true;
  }

  // The OpenAI client can retry after a quota failure and surface a later response
  // lookup/connection error. Only treat those as live-service noise after this
  // process has already observed quota exhaustion.
  return liveAiUsageLimitSeen && /Resource not found|Connection error/i.test(errorText);
}

function liveAiTest(name: string, fn: () => Promise<void>, timeout?: number) {
  test.skipIf(process.env.RUN_LIVE_AI !== "1")(
    name,
    async () => {
      try {
        await fn();
      } catch (error) {
        if (isLiveAiUnavailableError(error)) {
          console.warn(`Skipping ${name}: live AI service unavailable or usage limit reached`);
          return;
        }
        throw error;
      }
    },
    timeout
  );
}

liveAiTest("OpenAI custom fetch successfully makes a simple request", async () => {
  await setupTestUser();

  const openai = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  const model = CHAT_MODEL;
  const messages = [
    { role: "user", content: 'please reply with exactly and only the word "echo"' } as ChatMessage
  ];

  const stream = await openai.chat.completions.create({
    model,
    messages,
    stream: true
  });

  let fullResponse = "";

  for await (const chunk of stream) {
    const content = chunk.choices[0]?.delta?.content || "";
    fullResponse += content;
  }

  expect(fullResponse.trim()).toBe("echo");
});

async function streamCompletion(prompt: string) {
  const customFetch = createCustomFetch();
  let fullResponse = "";

  const response = await customFetch(`${API_URL}/v1/chat/completions`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Accept: "text/event-stream"
    },
    body: JSON.stringify({
      model: CHAT_MODEL,
      messages: [{ role: "user", content: prompt }],
      stream: true
    })
  });

  const text = await response.text();
  console.log("Full response text:", text);

  const lines = text.split("\n");
  for (const line of lines) {
    if (line.startsWith("data: ")) {
      const data = line.slice(6);
      if (data === "[DONE]") {
        break;
      }
      try {
        const parsed = JSON.parse(data);
        const content = parsed.choices[0]?.delta?.content;
        if (content) {
          console.log("Content:", content);
          fullResponse += content;
        }
      } catch (error) {
        console.error("Error parsing JSON:", error);
      }
    }
  }

  return fullResponse;
}

liveAiTest("streams chat completion", async () => {
  await setupTestUser();

  const response = await streamCompletion('please reply with exactly and only the word "echo"');
  console.log("Final response:", response);

  expect(response?.trim()).toBe("echo");
});

test.skip("text-to-speech with configured TTS model", async () => {
  await setupTestUser();

  const client = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  const textToSpeak = "Hello, this is a test of the text-to-speech system.";

  const response = await client.audio.speech.create({
    model: TTS_MODEL,
    voice: "af_sky" as any,
    input: textToSpeak,
    response_format: "mp3"
  });

  const buffer = Buffer.from(await response.arrayBuffer());

  // Verify we got back audio data
  expect(buffer.length).toBeGreaterThan(0);

  console.log(`TTS response size: ${buffer.length} bytes`);

  // Log the first 20 bytes to understand the format
  const first20Bytes = buffer.slice(0, 20).toString("hex");
  console.log(`First 20 bytes (hex): ${first20Bytes}`);

  // Also log as string to see if it's JSON or text
  const first100Chars = buffer.slice(0, 100).toString("utf-8");
  console.log(`First 100 chars (string): ${first100Chars}`);

  // MP3 files typically start with an ID3 tag or FF FB/FF FA (MPEG audio sync)
  const firstBytes = buffer.slice(0, 3).toString("hex");
  const isID3 = firstBytes === "494433"; // "ID3" in hex
  const isMPEGSync = firstBytes.startsWith("fff") || firstBytes.startsWith("ffe");

  // For now, just verify we got data back
  // The format check might need adjustment based on what the server returns
  if (!isID3 && !isMPEGSync) {
    console.warn("Response doesn't appear to be MP3 format, but got data back");
  }
});

// To run this test manually:
// bun test --test-name-pattern="Whisper transcription with real MP3 file" src/lib/test/integration/ai.test.ts --env-file .env.local
test.skip("Whisper transcription with real MP3 file", async () => {
  await setupTestUser();

  // Read the test MP3 file
  const mp3Path = new URL("./test-transcript.mp3", import.meta.url).pathname;
  const mp3Buffer = await Bun.file(mp3Path).arrayBuffer();
  const audioFile = new File([mp3Buffer], "test-transcript.mp3", { type: "audio/mpeg" });

  console.log(`Test MP3 file size: ${mp3Buffer.byteLength} bytes`);

  // Transcribe the MP3 file
  const transcriptionResult = await transcribeAudio(
    audioFile,
    "whisper-large-v3", // model
    "en" // language
  );

  console.log("Transcribed text from test MP3:", transcriptionResult.text);

  // Just verify we got some text back
  expect(transcriptionResult.text).toBeTruthy();
  expect(transcriptionResult.text.length).toBeGreaterThan(10);
}, 20000); // 20 second timeout for this test

test.skip("TTS → Whisper transcription chain", async () => {
  await setupTestUser();

  const client = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  // Step 1: Generate speech from simple text
  const originalText = "Hello";

  console.log("Generating speech from text:", originalText);

  const ttsResponse = await client.audio.speech.create({
    model: TTS_MODEL,
    voice: "af_sky" as any,
    input: originalText,
    response_format: "mp3"
  });

  const audioBuffer = Buffer.from(await ttsResponse.arrayBuffer());
  console.log(`Generated audio size: ${audioBuffer.length} bytes`);

  // Step 2: Create a Blob from the audio buffer
  const audioBlob = new Blob([audioBuffer], { type: "audio/mpeg" });
  const audioFile = new File([audioBlob], "tts_output.mp3", { type: "audio/mpeg" });

  // Step 3: Transcribe the audio back to text using Whisper
  console.log("Transcribing audio back to text...");

  const transcriptionResult = await transcribeAudio(
    audioFile,
    "whisper-large-v3", // model
    "en", // language
    undefined, // prompt
    0.0 // temperature
  );

  console.log("Transcribed text:", transcriptionResult.text);
  console.log("Original text:", originalText);

  // Just check that we got "hello" back (case-insensitive)
  expect(transcriptionResult.text.toLowerCase()).toContain("hello");
});

// TODO: Enable when backend supports stream: false for responses API
// Currently the backend always returns SSE stream regardless of stream parameter
test.skip("OpenAI responses endpoint returns in_progress status (non-streaming)", async () => {
  await setupTestUser();

  const openai = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  const response = await openai.responses.create({
    model: CHAT_MODEL,
    input: 'please reply with exactly and only the word "echo"',
    stream: false
  });

  console.log("Response:", response);

  // Check that the response has the expected structure
  expect(response).toHaveProperty("id");
  expect(response).toHaveProperty("object", "response");
  expect(response).toHaveProperty("status");

  // Since you mentioned the response is in_progress, let's check for that
  expect(response.status).toBe("in_progress");

  // TODO: Once the backend implements polling/completion for responses endpoint:
  // expect(response.output_text.trim()).toBe("echo");
});

liveAiTest("OpenAI responses endpoint streams response", async () => {
  await setupTestUser();

  const openai = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  // Create a conversation first (now required)
  const conversation = await openai.conversations.create({});

  const stream = await openai.responses.create({
    model: CHAT_MODEL,
    input: 'please reply with exactly and only the word "echo"',
    conversation: conversation.id,
    stream: true
  });

  let fullResponse = "";
  let hasInProgressEvent = false;
  let hasDeltaEvents = false;
  let hasDoneEvent = false;
  let eventCount = 0;

  try {
    for await (const event of stream) {
      eventCount++;

      if (event.type === "response.created" || event.type === "response.in_progress") {
        hasInProgressEvent = true;
      } else if (event.type === "response.output_text.delta") {
        hasDeltaEvents = true;
        // Text delta events have delta field
        if (event.delta) {
          fullResponse += event.delta;
        }
      } else if (event.type === "response.completed") {
        hasDoneEvent = true;
      }
    }
  } catch (error) {
    console.error("Error processing stream:", error);
    throw error;
  }

  // First check if we got any events at all
  expect(eventCount).toBeGreaterThan(0);

  // Verify we got all expected event types
  expect(hasInProgressEvent).toBe(true);
  expect(hasDeltaEvents).toBe(true);
  expect(hasDoneEvent).toBe(true);

  // Verify the accumulated response
  expect(fullResponse.trim()).toBe("echo");
});

liveAiTest("OpenAI responses endpoint validates complete event sequence", async () => {
  await setupTestUser();

  const openai = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  // Create a conversation first (now required)
  const conversation = await openai.conversations.create({});

  const stream = await openai.responses.create({
    model: CHAT_MODEL,
    input: 'please reply with exactly and only the words "echo echo"',
    conversation: conversation.id,
    stream: true
  });

  const events: any[] = [];
  const eventTypes: string[] = [];
  let fullResponse = "";

  try {
    for await (const event of stream) {
      events.push(event);
      eventTypes.push(event.type);

      if (event.type === "response.output_text.delta") {
        // Validate delta event structure
        expect(event).toHaveProperty("delta");
        expect(event).toHaveProperty("sequence_number");
        expect(event).toHaveProperty("item_id");
        expect(event).toHaveProperty("output_index");
        expect(event).toHaveProperty("content_index");
        expect(event).toHaveProperty("logprobs");

        fullResponse += event.delta;
      }
    }
  } catch (error) {
    console.error("Error processing stream:", error);
    throw error;
  }

  console.log("Received event types:", eventTypes);
  console.log("Total events:", events.length);

  // Validate we have the minimum required events
  expect(eventTypes).toContain("response.created");
  expect(eventTypes).toContain("response.output_text.delta");
  expect(eventTypes).toContain("response.completed");

  // Should have multiple events (not just 3)
  expect(events.length).toBeGreaterThan(3);

  // Validate we got multiple delta events (at least 2 for "echo echo")
  const deltaEvents = events.filter((e) => e.type === "response.output_text.delta");
  expect(deltaEvents.length).toBeGreaterThanOrEqual(2);

  // Validate first event has proper response structure
  const createdEvent = events.find((e) => e.type === "response.created");
  expect(createdEvent).toBeDefined();
  expect(createdEvent).toHaveProperty("sequence_number", 0);
  expect(createdEvent).toHaveProperty("response");
  expect(createdEvent.response).toHaveProperty("id");
  expect(createdEvent.response).toHaveProperty("status", "in_progress");
  expect(createdEvent.response).toHaveProperty("model");
  expect(createdEvent.response).toHaveProperty("output");
  expect(createdEvent.response.output).toEqual([]);

  // Check for other expected events
  const expectedEventTypes = [
    "response.created",
    "response.in_progress",
    "response.output_item.added",
    "response.content_part.added",
    "response.output_text.done",
    "response.content_part.done",
    "response.output_item.done",
    "response.completed"
  ];

  // Log missing events for debugging
  const missingEvents = expectedEventTypes.filter((et) => !eventTypes.includes(et));
  if (missingEvents.length > 0) {
    console.log("Missing event types:", missingEvents);
  }

  // Validate completed event has usage data
  const completedEvent = events.find((e) => e.type === "response.completed");
  expect(completedEvent).toBeDefined();
  expect(completedEvent).toHaveProperty("sequence_number");
  expect(completedEvent).toHaveProperty("response");
  expect(completedEvent.response).toHaveProperty("status", "completed");
  expect(completedEvent.response).toHaveProperty("usage");
  if (completedEvent.response.usage) {
    expect(completedEvent.response.usage).toHaveProperty("total_tokens");
    expect(completedEvent.response.usage).toHaveProperty("input_tokens");
    expect(completedEvent.response.usage).toHaveProperty("output_tokens");
  }

  // Verify sequence numbers are in order
  const sequenceNumbers = events
    .filter((e) => e.sequence_number !== undefined)
    .map((e) => e.sequence_number);

  for (let i = 1; i < sequenceNumbers.length; i++) {
    expect(sequenceNumbers[i]).toBeGreaterThan(sequenceNumbers[i - 1]);
  }

  // Verify the accumulated response
  expect(fullResponse.trim()).toBe("echo echo");
});

test("Conversations API: List conversations works with default parameters (custom endpoint)", async () => {
  await setupTestUser();

  // Note: list() is a custom OpenSecret extension, not part of standard OpenAI API
  const { listConversations } = await import("../../api");

  const conversationsList = await listConversations();

  expect(conversationsList).toBeDefined();
  expect(conversationsList.object).toBe("list");
  expect(Array.isArray(conversationsList.data)).toBe(true);
  expect(typeof conversationsList.has_more).toBe("boolean");

  // Check that each conversation has the correct structure
  if (conversationsList.data.length > 0) {
    const conversation = conversationsList.data[0];
    expect(conversation).toHaveProperty("id");
    expect(conversation).toHaveProperty("object", "conversation");
    expect(conversation).toHaveProperty("created_at");
    // Conversations may have metadata
    if (conversation.metadata) {
      expect(typeof conversation.metadata).toBe("object");
    }
  }
});

test("Conversations API: List conversations works with pagination parameters (custom endpoint)", async () => {
  await setupTestUser();

  // Note: list() is a custom OpenSecret extension, not part of standard OpenAI API
  const { listConversations } = await import("../../api");

  const conversationsList = await listConversations({ limit: 5 });

  expect(conversationsList).toBeDefined();
  expect(conversationsList.object).toBe("list");
  expect(Array.isArray(conversationsList.data)).toBe(true);
  expect(conversationsList.data.length).toBeLessThanOrEqual(5);
  expect(typeof conversationsList.has_more).toBe("boolean");

  if (conversationsList.data.length > 0) {
    expect(conversationsList.first_id).toBeDefined();
    expect(conversationsList.last_id).toBeDefined();
  }
});

liveAiTest("OpenAI responses retrieve endpoint works", async () => {
  await setupTestUser();

  const openai = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  // Create a conversation first (now required)
  const conversation = await openai.conversations.create({});

  // First create a response to retrieve
  const stream = await openai.responses.create({
    model: CHAT_MODEL,
    input: 'please reply with exactly and only the word "test"',
    conversation: conversation.id,
    stream: true
  });

  let responseId = "";

  // Get the response ID from the first event
  for await (const event of stream) {
    if (event.type === "response.created" && event.response?.id) {
      responseId = event.response.id;
      break;
    }
  }

  expect(responseId).not.toBe("");

  // Now retrieve the response
  const retrievedResponse = await openai.responses.retrieve(responseId);

  expect(retrievedResponse).toBeDefined();
  expect(retrievedResponse.id).toBe(responseId);
  expect(retrievedResponse.object).toBe("response");
  expect(retrievedResponse.created_at).toBeDefined();
  expect(retrievedResponse.status).toBeDefined();
  expect(retrievedResponse.model).toBeDefined();

  // If completed, should have usage and output
  if (retrievedResponse.status === "completed") {
    expect(retrievedResponse.usage).toBeDefined();
    expect(retrievedResponse.output).toBeDefined();
  }
});

// TODO: Re-enable this test once the cancel endpoint is correctly implemented on the backend
// Currently returns 400 (Bad Request) instead of proper cancellation handling
test.skip("OpenAI responses cancel endpoint works", async () => {
  await setupTestUser();

  const openai = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  // Create a long-running response that we can cancel
  const stream = await openai.responses.create({
    model: CHAT_MODEL,
    input: "write a very long story about space exploration with at least 500 words",
    stream: true
  });

  let responseId = "";

  // Get the response ID from the first event
  for await (const event of stream) {
    if (event.type === "response.created" && event.response?.id) {
      responseId = event.response.id;
      break; // Break immediately to catch it while in progress
    }
  }

  expect(responseId).not.toBe("");

  // Try to cancel the response (might already be completed depending on timing)
  try {
    const cancelledResponse = await openai.responses.cancel(responseId);

    expect(cancelledResponse).toBeDefined();
    expect(cancelledResponse.id).toBe(responseId);
    expect(cancelledResponse.object).toBe("response");
    expect(cancelledResponse.status).toBe("cancelled");
    expect(cancelledResponse.usage).toBeNull();
    expect(cancelledResponse.output).toBeNull();

    console.log("Successfully cancelled response");
  } catch (error) {
    // If we get a 422 error, it means the response wasn't in progress anymore
    // This is expected behavior for fast responses, so we'll verify the response completed
    if (
      error instanceof Error &&
      (error.message.includes("422") || error.message.includes("400"))
    ) {
      console.log("Response completed before cancel could be processed - checking final status");

      // Verify the response actually completed
      const completedResponse = await openai.responses.retrieve(responseId);
      expect(completedResponse.status).toBe("completed");
      expect(completedResponse.output).toBeDefined();

      console.log("Confirmed response completed successfully");
    } else {
      throw error;
    }
  }
});

liveAiTest(
  "OpenAI responses delete endpoint works",
  async () => {
    await setupTestUser();

    const openai = new OpenAI({
      baseURL: `${API_URL}/v1/`,
      dangerouslyAllowBrowser: true,
      apiKey: "api-key-doesnt-matter",
      defaultHeaders: {
        "Accept-Encoding": "identity"
      },
      fetch: createCustomFetch()
    });

    // Create a conversation first (now required)
    const conversation = await openai.conversations.create({});

    // First create a response to delete
    const stream = await openai.responses.create({
      model: CHAT_MODEL,
      input: 'please reply with exactly and only the word "delete"',
      conversation: conversation.id,
      stream: true
    });

    let responseId = "";

    // Get the response ID and let it complete
    for await (const event of stream) {
      if (event.type === "response.created" && event.response?.id) {
        responseId = event.response.id;
      }
      // Continue until completion for clean deletion
    }

    expect(responseId).not.toBe("");

    // Verify the response exists first
    const existingResponse = await openai.responses.retrieve(responseId);
    expect(existingResponse.id).toBe(responseId);
    console.log(`Response ${responseId} exists with status: ${existingResponse.status}`);

    // Now delete the response
    await openai.responses.delete(responseId);

    console.log(`Successfully deleted response ${responseId}`);

    // Verify the response is actually deleted by trying to retrieve it
    try {
      await openai.responses.retrieve(responseId);
      throw new Error("Should have thrown 404 error for deleted response");
    } catch (error) {
      // Should get 404 error for deleted response
      expect(error instanceof Error).toBe(true);
      // The error could be a "Connection error." from OpenAI SDK or contain "404"
      const errorMessage = (error as Error).message;
      const isExpectedError =
        errorMessage.includes("404") || errorMessage.includes("Connection error");
      expect(isExpectedError).toBe(true);
      console.log(`Confirmed response was deleted - error received: ${errorMessage}`);
    }
  },
  10000
);

liveAiTest("Integration test: Complete responses workflow", async () => {
  await setupTestUser();

  const openai = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  // Create a conversation first (now required)
  const conversation = await openai.conversations.create({});

  // 1. Create a new response
  const stream = await openai.responses.create({
    model: CHAT_MODEL,
    input: 'please reply with exactly and only the word "workflow"',
    conversation: conversation.id,
    stream: true
  });

  let responseId = "";
  let fullResponse = "";

  // 2. Process the streaming response
  for await (const event of stream) {
    if (event.type === "response.created" && event.response?.id) {
      responseId = event.response.id;
    }
    if (event.type === "response.output_text.delta" && event.delta) {
      fullResponse += event.delta;
    }
  }

  expect(responseId).not.toBe("");
  expect(fullResponse.trim()).toBe("workflow");

  // 3. Since responses list has been removed, we'll verify using the response retrieve
  // The response itself should exist and be retrievable

  // 4. Retrieve the full response
  const retrievedResponse = await openai.responses.retrieve(responseId);
  expect(retrievedResponse.id).toBe(responseId);
  expect(retrievedResponse.status).toBe("completed");
  expect(Array.isArray(retrievedResponse.output)).toBe(true);
  expect(retrievedResponse.output?.length).toBeGreaterThan(0);

  const firstItem = retrievedResponse.output![0];
  expect(firstItem.type).toBe("message");
  if (firstItem.type === "message") {
    const content = firstItem.content[0];
    if ("text" in content) {
      expect(content.text).toContain("workflow");
    }
  }
  expect(retrievedResponse.usage).toBeDefined();

  // 5. Delete the response
  await openai.responses.delete(responseId);

  // 6. After deletion, the response should no longer be retrievable
  // We'll verify this in the deletion test
});

liveAiTest(
  "Integration test: Direct API functions for responses",
  async () => {
    await setupTestUser();

    const openai = new OpenAI({
      baseURL: `${API_URL}/v1/`,
      dangerouslyAllowBrowser: true,
      apiKey: "api-key-doesnt-matter",
      defaultHeaders: {
        "Accept-Encoding": "identity"
      },
      fetch: createCustomFetch()
    });

    const { fetchResponse, deleteResponse } = await import("../../api");

    // Create a conversation first (now required)
    const conversation = await openai.conversations.create({});

    // 1. Create a response to test with
    const stream = await openai.responses.create({
      model: CHAT_MODEL,
      input: 'please reply with exactly and only the word "apitest"',
      conversation: conversation.id,
      stream: true
    });

    let responseId = "";
    for await (const event of stream) {
      if (event.type === "response.created" && event.response?.id) {
        responseId = event.response.id;
      }
      if (event.type === "response.completed") {
        break;
      }
    }

    expect(responseId).not.toBe("");

    // 2. Test fetchResponse - should return full details
    let retrievedResponse = await fetchResponse(responseId);
    const terminalStatuses = new Set(["completed", "failed", "cancelled"]);
    for (
      let attempt = 0;
      attempt < 34 && !terminalStatuses.has(retrievedResponse.status);
      attempt++
    ) {
      await new Promise((resolve) => setTimeout(resolve, 300));
      retrievedResponse = await fetchResponse(responseId);
    }

    expect(retrievedResponse.id).toBe(responseId);
    expect(retrievedResponse.object).toBe("response");
    expect(terminalStatuses.has(retrievedResponse.status)).toBe(true);
    expect(retrievedResponse.status).toBe("completed");
    expect(Array.isArray(retrievedResponse.output)).toBe(true);
    expect(retrievedResponse.output?.length).toBeGreaterThan(0);

    const firstItem = (retrievedResponse.output as any[])[0];
    expect(firstItem.type).toBe("message");
    expect(firstItem.content[0].text).toContain("apitest");

    expect(retrievedResponse.usage).toBeDefined();
    expect(retrievedResponse.usage?.input_tokens).toBeGreaterThan(0);
    expect(retrievedResponse.usage?.output_tokens).toBeGreaterThan(0);

    // 3. Test deleteResponse - should return deletion confirmation
    const deleteResult = await deleteResponse(responseId);
    expect(deleteResult.id).toBe(responseId);
    expect(deleteResult.object).toBe("response.deleted");
    expect(deleteResult.deleted).toBe(true);

    // 4. Verify response is deleted by trying to fetch it
    try {
      await fetchResponse(responseId);
      throw new Error("Should have thrown error for deleted response");
    } catch (error) {
      expect(error instanceof Error).toBe(true);
      const errorMessage = (error as Error).message;
      // The API returns "Resource not found" when trying to fetch a deleted response
      expect(errorMessage).toContain("Resource not found");
    }
  },
  10000
);

liveAiTest("Integration test: Cancel in-progress response", async () => {
  await setupTestUser();

  const openai = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  const { cancelResponse, fetchResponse, deleteResponse } = await import("../../api");

  // Create a conversation first (now required)
  const conversation = await openai.conversations.create({});

  // Create a slow response that we can cancel
  const stream = await openai.responses.create({
    model: CHAT_MODEL,
    input: "Count from 1 to 100 slowly, with detailed explanations for each number",
    conversation: conversation.id,
    stream: true
  });

  let responseId = "";
  let eventCount = 0;

  // Start streaming but stop early to cancel
  for await (const event of stream) {
    if (event.type === "response.created" && event.response?.id) {
      responseId = event.response.id;
    }
    eventCount++;
    // Stop after a few events to ensure it's still in progress
    if (eventCount > 3) {
      break;
    }
  }

  expect(responseId).not.toBe("");

  // Try to cancel the response
  try {
    const cancelResult = await cancelResponse(responseId);
    expect(cancelResult.id).toBe(responseId);
    expect(cancelResult.object).toBe("response");
    expect(cancelResult.status).toBe("cancelled");

    // Verify the response shows as cancelled
    const cancelledResponse = await fetchResponse(responseId);
    expect(cancelledResponse.status).toBe("cancelled");
  } catch (error) {
    // Response might have completed already, which is okay
    console.log("Could not cancel response, it may have completed:", error);

    // Check if it completed
    const response = await fetchResponse(responseId);
    expect(response.status).toBe("completed");
  }

  // Clean up
  await deleteResponse(responseId);
});

test("Conversations API: Create, Get, Update, Delete conversation", async () => {
  await setupTestUser();

  const openai = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  // 1. Create a conversation
  const conversation = await openai.conversations.create({
    metadata: {
      title: "Test Conversation",
      category: "testing"
    }
  });

  expect(conversation).toBeDefined();
  expect(conversation.id).toBeDefined();
  expect(conversation.object).toBe("conversation");
  expect(conversation.created_at).toBeDefined();
  expect(conversation.metadata).toEqual({
    title: "Test Conversation",
    category: "testing"
  });

  // 2. Retrieve the conversation
  const retrieved = await openai.conversations.retrieve(conversation.id);
  expect(retrieved.id).toBe(conversation.id);
  expect(retrieved.metadata).toEqual(conversation.metadata);

  // 3. Update the conversation metadata
  const updated = await openai.conversations.update(conversation.id, {
    metadata: {
      title: "Updated Test Conversation",
      category: "testing",
      status: "active"
    }
  });
  expect(updated.id).toBe(conversation.id);
  expect((updated.metadata as any).title).toBe("Updated Test Conversation");
  expect((updated.metadata as any).status).toBe("active");

  // 4. Delete the conversation
  const deleteResult = await openai.conversations.delete(conversation.id);
  expect(deleteResult.id).toBe(conversation.id);
  expect(deleteResult.object).toBe("conversation.deleted");
  expect(deleteResult.deleted).toBe(true);
});

liveAiTest("Responses API: Create response with conversation parameter", async () => {
  await setupTestUser();

  const openai = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  // First create a conversation using OpenAI client
  const conversation = await openai.conversations.create({
    metadata: {
      title: "Test Conversation for Responses"
    }
  });

  // Create first response in the conversation
  const stream1 = await openai.responses.create({
    model: CHAT_MODEL,
    input: 'please reply with exactly and only the word "first"',
    conversation: conversation.id,
    stream: true
  });

  let responseId1 = "";
  let fullResponse1 = "";

  for await (const event of stream1) {
    if (event.type === "response.created" && event.response?.id) {
      responseId1 = event.response.id;
    }
    if (event.type === "response.output_text.delta") {
      fullResponse1 += event.delta;
    }
  }

  expect(responseId1).not.toBe("");
  expect(fullResponse1.trim()).toBe("first");

  // Create second response in the same conversation using object format
  const stream2 = await openai.responses.create({
    model: CHAT_MODEL,
    input: 'please reply with exactly and only the word "second"',
    conversation: { id: conversation.id }, // Test object format
    stream: true
  });

  let responseId2 = "";
  let fullResponse2 = "";

  for await (const event of stream2) {
    if (event.type === "response.created" && event.response?.id) {
      responseId2 = event.response.id;
    }
    if (event.type === "response.output_text.delta") {
      fullResponse2 += event.delta;
    }
  }

  expect(responseId2).not.toBe("");
  expect(fullResponse2.trim()).toBe("second");
  expect(responseId2).not.toBe(responseId1);

  // Clean up
  await openai.responses.delete(responseId1);
  await openai.responses.delete(responseId2);
  await openai.conversations.delete(conversation.id);
});

liveAiTest("Conversations API: Full integration with responses", async () => {
  await setupTestUser();

  const openai = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  // Using OpenAI client for responses since non-streaming is not supported yet

  // 1. Create a conversation
  const conversation = await openai.conversations.create({
    metadata: {
      title: "Integration Test Conversation"
    }
  });

  // 2. Create responses in the conversation (using streaming since non-streaming not supported yet)
  const stream = await openai.responses.create({
    model: CHAT_MODEL,
    input: "What is 2+2?",
    conversation: conversation.id,
    stream: true
  });

  let responseId = "";
  for await (const event of stream) {
    if (event.type === "response.created" && event.response?.id) {
      responseId = event.response.id;
      break;
    }
  }

  const response1 = { id: responseId };

  expect(response1).toBeDefined();

  // 3. List conversation items (should include the messages)
  const items = await openai.conversations.items.list(conversation.id);
  expect(items.data.length).toBeGreaterThan(0);

  // 4. Update conversation metadata
  const updated = await openai.conversations.update(conversation.id, {
    metadata: {
      title: "Math Questions",
      status: "completed"
    }
  });
  expect((updated.metadata as any).status).toBe("completed");

  // 5. List conversations using raw API (since it's non-standard)
  const { listConversations } = await import("../../api");
  const conversations = await listConversations({ limit: 10 });
  const ourConv = conversations.data.find((c) => c.id === conversation.id);
  expect(ourConv).toBeDefined();
  expect(ourConv?.metadata?.status).toBe("completed");

  // 6. Clean up
  await openai.conversations.delete(conversation.id);
});

test("Integration test: Delete all conversations", async () => {
  await setupTestUser();

  const openai = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  // Create a couple of conversations
  await openai.conversations.create({});
  await openai.conversations.create({});

  // Verify we have conversations
  const initialList = await listConversations();
  expect(initialList.data.length).toBeGreaterThanOrEqual(2);

  // Delete all conversations
  const result = await deleteConversations();

  expect(result.object).toBe("list.deleted");
  expect(result.deleted).toBe(true);

  // Verify deletion
  const finalList = await listConversations();
  expect(finalList.data.length).toBe(0);
});

test("Integration test: Batch delete conversations", async () => {
  await setupTestUser();

  // Clean up any existing conversations first
  await deleteConversations();

  const openai = new OpenAI({
    baseURL: `${API_URL}/v1/`,
    dangerouslyAllowBrowser: true,
    apiKey: "api-key-doesnt-matter",
    defaultHeaders: {
      "Accept-Encoding": "identity"
    },
    fetch: createCustomFetch()
  });

  // Create three conversations
  const conv1 = await openai.conversations.create({});
  const conv2 = await openai.conversations.create({});
  const conv3 = await openai.conversations.create({});

  // Verify we have 3 conversations
  const initialList = await listConversations();
  expect(initialList.data.length).toBe(3);

  // Batch delete only the first two
  const result = await batchDeleteConversations([conv1.id, conv2.id]);

  expect(result.object).toBe("list");
  expect(result.data.length).toBe(2);
  expect(result.data[0].deleted).toBe(true);
  expect(result.data[0].object).toBe("conversation.deleted");
  expect(result.data[1].deleted).toBe(true);

  // Verify only one conversation remains
  const afterBatchDelete = await listConversations();
  expect(afterBatchDelete.data.length).toBe(1);
  expect(afterBatchDelete.data[0].id).toBe(conv3.id);

  // Test batch delete with a non-existent ID
  const resultWithNotFound = await batchDeleteConversations([
    conv3.id,
    "00000000-0000-0000-0000-000000000000"
  ]);

  expect(resultWithNotFound.data.length).toBe(2);
  // One should succeed
  const successItem = resultWithNotFound.data.find((item) => item.id === conv3.id);
  expect(successItem?.deleted).toBe(true);
  // One should fail with not_found
  const failedItem = resultWithNotFound.data.find(
    (item) => item.id === "00000000-0000-0000-0000-000000000000"
  );
  expect(failedItem?.deleted).toBe(false);
  expect(failedItem?.error).toBe("not_found");

  // Verify all conversations are now deleted
  const finalList = await listConversations();
  expect(finalList.data.length).toBe(0);
});

test("System One: rejections keep status and code with JWT and API key; answers are typed when a route exists", async () => {
  await setupTestUser();

  // Validation runs before billing and the provider, so this is deterministic
  // on any backend.
  const oneOption: SystemOneRequest = {
    state: "s",
    questions: { q: { type: "choice", instructions: "x", criteria: { only: null } } }
  };
  const rejection = await systemOne(oneOption).catch((error: unknown) => error);
  expect(rejection).toBeInstanceOf(SystemOneError);
  expect((rejection as SystemOneError).status).toBe(422);
  expect((rejection as SystemOneError).code).toBe("system_one_bad_option_count");

  const keyName = `system-one-${Date.now()}`;
  const created = await createApiKey(keyName);
  try {
    const withKey = await systemOne(oneOption, { apiKey: created.key }).catch(
      (error: unknown) => error
    );
    expect(withKey).toBeInstanceOf(SystemOneError);
    expect((withKey as SystemOneError).status).toBe(422);
    expect((withKey as SystemOneError).code).toBe("system_one_bad_option_count");
  } finally {
    await deleteApiKey(keyName);
  }

  const ticket: SystemOneRequest = {
    state: { ticket: "Payment went through twice and I need one refunded today." },
    questions: {
      is_urgent: { type: "noul", instructions: "Does the customer need help today?" },
      intent: {
        type: "choice",
        instructions: "What does the customer want?",
        criteria: { refund: "money back", question: "information", praise: "thanks" }
      },
      frustration: {
        type: "score",
        instructions: "How frustrated is the customer?",
        criteria: ["Low", "Medium", "High"]
      }
    }
  };
  const result = await systemOne(ticket).catch((error: unknown) => error);
  if (result instanceof SystemOneError && (result.status === 503 || result.status === 403)) {
    // No Continuum route (CI) or no entitled plan: the typed error is the contract here.
    console.log(
      `System One answers skipped: ${result.status} ${result.code ?? ""} ${result.message}`
    );
    return;
  }
  const response = result as SystemOneResponse;
  expect(Object.keys(response.answers)).toEqual(["is_urgent", "intent", "frustration"]);
  expect(response.answers.is_urgent.type).toBe("noul");
  const intent = response.answers.intent;
  expect(intent.type).toBe("choice");
  if (intent.type === "choice") {
    expect(Object.keys(intent.probabilities)).toEqual(["refund", "question", "praise"]);
    expect(intent.probabilities[intent.choice]).toBeGreaterThan(0);
    expect(intent.confidence).toBeGreaterThanOrEqual(0);
  }
  const frustration = response.answers.frustration;
  expect(frustration.type).toBe("score");
  if (frustration.type === "score") {
    expect(Object.keys(frustration.legend)).toEqual(["0", "1", "2"]);
    expect(frustration.score).toBeGreaterThanOrEqual(0);
    expect(frustration.score).toBeLessThanOrEqual(2);
  }
  expect(response.usage.requests).toBeGreaterThanOrEqual(3);
});
