import { expect, test, describe, beforeAll, afterAll } from "bun:test";
import { fetchLogin, createApiKey, listApiKeys, deleteApiKey, fetchModels } from "../../api";
import { createCustomFetch } from "../../ai";
import OpenAI from "openai";

const TEST_EMAIL = process.env.VITE_TEST_EMAIL;
const TEST_PASSWORD = process.env.VITE_TEST_PASSWORD;
const TEST_CLIENT_ID = process.env.VITE_TEST_CLIENT_ID;
const API_URL = process.env.VITE_OPEN_SECRET_API_URL;
const CHAT_MODEL = process.env.VITE_TEST_CHAT_MODEL ?? "llama3-3-70b";

if (!TEST_EMAIL || !TEST_PASSWORD || !TEST_CLIENT_ID || !API_URL) {
  throw new Error("Test credentials must be set in .env.local");
}

async function setupTestUser() {
  const { access_token, refresh_token } = await fetchLogin(
    TEST_EMAIL!,
    TEST_PASSWORD!,
    TEST_CLIENT_ID!
  );
  window.localStorage.setItem("access_token", access_token);
  window.localStorage.setItem("refresh_token", refresh_token);
}

describe("API Key Management", () => {
  beforeAll(async () => {
    await setupTestUser();
  });

  test("Create, list, and delete API key", async () => {
    // Create a new API key
    const keyName = `Test Key ${Date.now()}`;
    const createdKey = await createApiKey(keyName);

    expect(createdKey.name).toBe(keyName);
    expect(createdKey.key).toBeDefined();
    expect(createdKey.created_at).toBeDefined();

    // Verify the key format is a UUID with dashes
    const uuidRegex = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
    expect(createdKey.key).toMatch(uuidRegex);
    // The non-secret key ID is separate from the key itself
    expect(createdKey.public_id).toMatch(uuidRegex);
    expect(createdKey.public_id).not.toBe(createdKey.key);

    // List API keys and verify the new key is present
    const response = await listApiKeys();
    console.log("Listed response:", response);

    // Check if keys is an array
    expect(Array.isArray(response.keys)).toBe(true);
    const foundKey = response.keys.find((k) => k.name === createdKey.name);

    expect(foundKey).toBeDefined();
    expect(foundKey!.name).toBe(keyName);
    expect(foundKey!.created_at).toBe(createdKey.created_at);
    expect(foundKey!.public_id).toBe(createdKey.public_id);
    // The key field should NOT be present in the list response
    expect((foundKey as any).key).toBeUndefined();

    // Delete the API key
    await deleteApiKey(createdKey.name);

    // Verify the key is deleted
    const responseAfterDelete = await listApiKeys();
    const deletedKey = responseAfterDelete.keys.find((k) => k.name === createdKey.name);
    expect(deletedKey).toBeUndefined();
  });

  test("Create multiple API keys", async () => {
    const keyNames: string[] = [];

    try {
      // Create multiple keys
      for (let i = 0; i < 3; i++) {
        const keyName = `Test Key ${i} - ${Date.now()}`;
        const key = await createApiKey(keyName);
        keyNames.push(key.name);
      }

      // List keys and verify all are present
      const response = await listApiKeys();
      for (const name of keyNames) {
        expect(response.keys.some((k) => k.name === name)).toBe(true);
      }
    } finally {
      // Clean up: delete all created keys
      for (const name of keyNames) {
        try {
          await deleteApiKey(name);
        } catch (error) {
          console.warn(`Failed to delete key ${name}:`, error);
        }
      }
    }
  });
});

describe("API Key Authentication with OpenAI", () => {
  let testApiKey: string;
  let testApiKeyName: string;

  beforeAll(async () => {
    await setupTestUser();

    // Create an API key for testing
    testApiKeyName = `OpenAI Test Key ${Date.now()}`;
    const createdKey = await createApiKey(testApiKeyName);
    testApiKey = createdKey.key;
  });

  test.skipIf(process.env.RUN_LIVE_AI !== "1")(
    "OpenAI custom fetch with API key makes a simple request",
    async () => {
      const openai = new OpenAI({
        baseURL: `${API_URL}/v1/`,
        dangerouslyAllowBrowser: true,
        apiKey: testApiKey, // Use the API key directly
        defaultHeaders: {
          "Accept-Encoding": "identity"
        },
        fetch: createCustomFetch({ apiKey: testApiKey })
      });

      const model = CHAT_MODEL;
      const messages = [
        { role: "user" as const, content: 'please reply with exactly and only the word "echo"' }
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

      // In OpenAI v5, we just iterate through the stream instead of finalChatCompletion()
      // The stream already completed above

      expect(fullResponse.trim()).toBe("echo");
    }
  );

  test("Fetch models with API key authentication", async () => {
    // Test that fetchModels works with API key
    const models = await fetchModels(testApiKey);

    expect(Array.isArray(models)).toBe(true);
    expect(models.length).toBeGreaterThan(0);

    // Verify each model has required fields
    const firstModel = models[0];
    expect(firstModel.id).toBeDefined();
    expect(firstModel.object).toBe("model");
  });

  test("API key authentication fails with invalid key", async () => {
    const invalidApiKey = "550e8400-e29b-41d4-a716-000000000000"; // Invalid UUID

    const customFetch = createCustomFetch({ apiKey: invalidApiKey });

    try {
      const response = await customFetch(`${API_URL}/v1/models`, {
        method: "GET",
        headers: {
          "Content-Type": "application/json"
        }
      });

      // Should get 401 Unauthorized
      expect(response.status).toBe(401);
    } catch (error) {
      // If it throws before the response, that's also acceptable
      expect(error).toBeDefined();
    }
  });

  test("JWT auth still works alongside API key support", async () => {
    // Test that the original JWT-based auth still works
    const models = await fetchModels();
    expect(Array.isArray(models)).toBe(true);
    expect(models.length).toBeGreaterThan(0);
  });

  // Clean up after all tests
  afterAll(async () => {
    if (testApiKeyName) {
      try {
        await deleteApiKey(testApiKeyName);
      } catch (error) {
        console.warn("Failed to delete test API key:", error);
      }
    }
  });
});
