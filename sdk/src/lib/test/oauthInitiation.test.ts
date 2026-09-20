import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import {
  getApiPcrConfig,
  getApiUrl,
  initiateAppleAuth,
  initiateGitHubAuth,
  initiateGoogleAuth,
  setApiUrl
} from "../api";
import type { PcrConfig } from "../pcr";
import {
  transportV2Runtime,
  type TransportV2OAuthProvider,
  type TransportV2RuntimeRequest
} from "../transportV2/runtime";

const API_URL = "https://oauth-initiation.example.test/backend";
const CLIENT_ID = "00000000-0000-4000-8000-000000000001";
const STATE = "opaque-backend-state-with-selected-callback";
const SELECTED_REDIRECT = "https://AUTH.example.test:443/auth/%63allback?b=2&a=1";

type SeenRequest = {
  apiUrl: string;
  request: TransportV2RuntimeRequest["request"];
  body: unknown;
};

let seen: SeenRequest[];
let continuations: Array<{ provider: TransportV2OAuthProvider; state: string }>;
let respond: () => Response;
let restoreRequest: () => void;
let previousApiUrl: string;
let previousPcrConfig: PcrConfig;

function jsonResponse(value: unknown, init?: ResponseInit): Response {
  return new Response(JSON.stringify(value), {
    ...init,
    headers: { "content-type": "application/json", ...init?.headers }
  });
}

beforeEach(() => {
  previousApiUrl = getApiUrl();
  previousPcrConfig = getApiPcrConfig();
  setApiUrl(API_URL, { environment: "development" });
  seen = [];
  continuations = [];
  respond = () =>
    jsonResponse({ auth_url: "https://provider.example.test/authorize", state: STATE });
  const request = spyOn(transportV2Runtime, "request").mockImplementation(async (input) => {
    // The production encrypted API clears these bytes after the call completes.
    // Capture a copy at its transport boundary rather than retaining that buffer.
    const body = input.request.body ? new Uint8Array(input.request.body) : undefined;
    seen.push({
      apiUrl: input.apiUrl,
      request: { ...input.request, body },
      body: body ? JSON.parse(new TextDecoder().decode(body)) : undefined
    });
    return {
      response: respond(),
      rememberOAuthContinuation(provider, state) {
        continuations.push({ provider, state });
      }
    };
  });
  restoreRequest = () => request.mockRestore();
});

afterEach(() => {
  restoreRequest();
  setApiUrl(previousApiUrl, previousPcrConfig);
});

for (const { provider, initiate } of [
  { provider: "github", initiate: initiateGitHubAuth },
  { provider: "google", initiate: initiateGoogleAuth },
  { provider: "apple", initiate: initiateAppleAuth }
] as const) {
  describe(`${provider} OAuth initiation`, () => {
    test("omits optional wire fields for existing callers and keeps the opaque continuation", async () => {
      const result = await initiate(CLIENT_ID);
      await initiate(CLIENT_ID, undefined, undefined);
      await initiate(CLIENT_ID, "");

      expect(seen.map(({ body }) => body)).toEqual([
        { client_id: CLIENT_ID },
        { client_id: CLIENT_ID },
        { client_id: CLIENT_ID }
      ]);
      expect(seen[0]).toMatchObject({
        apiUrl: API_URL,
        request: {
          method: "POST",
          target: `/auth/${provider}`,
          headers: [{ name: "content-type", value: "application/json" }]
        }
      });
      expect(seen[0].request.credential).toBeUndefined();
      expect(result).toEqual({
        auth_url: "https://provider.example.test/authorize",
        ...(provider === "apple" ? { state: STATE } : { csrf_token: STATE })
      });
      expect(continuations).toEqual(Array(3).fill({ provider, state: STATE }));
    });

    test("preserves the invite-only request", async () => {
      await initiate(CLIENT_ID, "existing-invite");
      expect(seen[0].body).toEqual({ client_id: CLIENT_ID, invite_code: "existing-invite" });
    });

    test("sends the selected callback unchanged alongside the invite", async () => {
      await initiate(CLIENT_ID, "existing-invite", SELECTED_REDIRECT);
      expect(seen[0].body).toEqual({
        client_id: CLIENT_ID,
        invite_code: "existing-invite",
        redirect_url: SELECTED_REDIRECT
      });
    });

    test("accepts a callback selection without adding an invite", async () => {
      await initiate(CLIENT_ID, undefined, SELECTED_REDIRECT);
      expect(seen[0].body).toEqual({ client_id: CLIENT_ID, redirect_url: SELECTED_REDIRECT });
    });

    test("leaves callback validation to the backend, including an explicit empty string", async () => {
      await initiate(CLIENT_ID, undefined, "");
      expect(seen[0].body).toEqual({ client_id: CLIENT_ID, redirect_url: "" });
    });

    test("preserves backend rejection status, message, and headers", async () => {
      respond = () =>
        jsonResponse(
          { error: "Callback selection rejected" },
          { status: 400, headers: { "x-test-error": "callback-rejected" } }
        );
      const error = await initiate(CLIENT_ID, undefined, SELECTED_REDIRECT).catch(
        (caught: unknown) => caught
      );
      expect(error).toBeInstanceOf(Error);
      expect(error).toMatchObject({ message: "Callback selection rejected", status: 400 });
      expect((error as Error & { headers: Headers }).headers.get("x-test-error")).toBe(
        "callback-rejected"
      );
      expect(continuations).toEqual([]);
    });

    test("retains the existing invalid-invite explanation", async () => {
      respond = () => jsonResponse({ error: "Invalid invite code" }, { status: 400 });
      await expect(initiate(CLIENT_ID, "invalid-invite", SELECTED_REDIRECT)).rejects.toThrow(
        "Invalid invite code. Please check and try again."
      );
      expect(continuations).toEqual([]);
    });

    test("preserves transport failure messages", async () => {
      respond = () => {
        throw new Error("Transport failed before provider initiation");
      };
      await expect(initiate(CLIENT_ID, undefined, SELECTED_REDIRECT)).rejects.toThrow(
        "Transport failed before provider initiation"
      );
      expect(continuations).toEqual([]);
    });
  });
}
