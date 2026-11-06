import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import {
  completePasswordReset,
  confirmPasswordReset,
  disableRecoveryCode,
  enrollRecoveryCode,
  fetchPasswordResetOptions,
  fetchRecoveryCodeStatus,
  getApiPcrConfig,
  getApiUrl,
  rotateRecoveryCode,
  setApiUrl
} from "../api";
import type { PcrConfig } from "../pcr";
import { installTransportV2Credentials, readTransportV2Credentials } from "../transportV2/auth";
import { transportV2Runtime, type TransportV2RuntimeRequest } from "../transportV2/runtime";
import { RecoveryCodeFormatError } from "../recoveryCode";

const API_URL = "https://recovery.example.test/backend";
const CLIENT_ID = "00000000-0000-4000-8000-000000000001";
const EMAIL = "user@example.test";
const RESET_CODE = "ABCD1234";
const RESET_SECRET = "client-reset-secret";
const RECOVERY_CODE =
  "MPLRC1-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-39YZ-VTQZ";
const PROOF = {
  email: EMAIL,
  alphanumeric_code: RESET_CODE,
  plaintext_secret: RESET_SECRET,
  client_id: CLIENT_ID
};

type SeenRequest = {
  request: TransportV2RuntimeRequest["request"];
  body: unknown;
};

let seen: SeenRequest[];
let respond: () => Response;
let restoreRequest: () => void;
let previousApiUrl: string;
let previousPcrConfig: PcrConfig;

function base64Url(value: string | Uint8Array): string {
  return Buffer.from(value).toString("base64url");
}

function userToken(purpose: "access" | "refresh", principalId: string): string {
  const audience =
    purpose === "access"
      ? "urn:opensecret:internal:transport-v2:user:access-token"
      : "urn:opensecret:internal:transport-v2:user:refresh-token";
  return [
    base64Url(JSON.stringify({ alg: "ES256K", typ: "JWT" })),
    base64Url(JSON.stringify({ aud: audience, sub: principalId, exp: 2_000_000_000, tf: 2 })),
    base64Url(new Uint8Array(64).fill(0x5a))
  ].join(".");
}

function jsonResponse(value: unknown, init?: ResponseInit): Response {
  return new Response(JSON.stringify(value), {
    ...init,
    headers: { "content-type": "application/json", ...init?.headers }
  });
}

beforeEach(() => {
  globalThis.localStorage.clear();
  previousApiUrl = getApiUrl();
  previousPcrConfig = getApiPcrConfig();
  setApiUrl(API_URL, { environment: "development" });
  seen = [];
  respond = () => jsonResponse({});
  const request = spyOn(transportV2Runtime, "request").mockImplementation(async (input) => {
    // The encrypted API clears request bytes after the call; keep a copy.
    const body = input.request.body ? new Uint8Array(input.request.body) : undefined;
    seen.push({
      request: { ...input.request, body },
      body: body ? JSON.parse(new TextDecoder().decode(body)) : undefined
    });
    return { response: respond(), rememberOAuthContinuation() {} };
  });
  restoreRequest = () => request.mockRestore();
});

afterEach(() => {
  restoreRequest();
  setApiUrl(previousApiUrl, previousPcrConfig);
  globalThis.localStorage.clear();
});

describe("V2 password reset", () => {
  test("options sends the reset proof anonymously", async () => {
    respond = () => jsonResponse({ recovery_enrolled: true, destructive_reset_available: true });

    const options = await fetchPasswordResetOptions(EMAIL, RESET_CODE, RESET_SECRET, CLIENT_ID);

    expect(options).toEqual({ recovery_enrolled: true, destructive_reset_available: true });
    expect(seen).toHaveLength(1);
    expect(seen[0].request).toMatchObject({ method: "POST", target: "/password-reset/v2/options" });
    expect(seen[0].request.credential).toBeUndefined();
    expect(seen[0].body).toEqual({ proof: PROOF });
  });

  test("preserving completion sends the recovery code and installs the new tokens", async () => {
    const accessToken = userToken("access", "user-1");
    const refreshToken = userToken("refresh", "user-1");
    respond = () =>
      jsonResponse({
        message: "Password reset successful.",
        access_token: accessToken,
        refresh_token: refreshToken
      });

    await completePasswordReset(EMAIL, RESET_CODE, RESET_SECRET, "new-password", CLIENT_ID, {
      mode: "preserve",
      recoveryCode: RECOVERY_CODE
    });

    expect(seen[0].request).toMatchObject({
      method: "POST",
      target: "/password-reset/v2/complete"
    });
    expect(seen[0].request.credential).toBeUndefined();
    expect(seen[0].body).toEqual({
      proof: PROOF,
      new_password: "new-password",
      mode: { preserve: { recovery_code: RECOVERY_CODE } }
    });
    expect(readTransportV2Credentials(API_URL, "user")).toMatchObject({
      principalId: "user-1",
      accessToken,
      refreshToken
    });
  });

  test("destructive completion sends the data-loss acknowledgment", async () => {
    respond = () =>
      jsonResponse({
        message: "Password reset successful.",
        access_token: userToken("access", "user-1"),
        refresh_token: userToken("refresh", "user-1")
      });

    await completePasswordReset(EMAIL, RESET_CODE, RESET_SECRET, "new-password", CLIENT_ID, {
      mode: "destructive",
      acknowledgeDataLoss: true
    });

    expect(seen[0].body).toEqual({
      proof: PROOF,
      new_password: "new-password",
      mode: { destructive: { acknowledge_data_loss: true } }
    });
  });

  test("normalizes a standalone recovery code before sending", async () => {
    respond = () =>
      jsonResponse({
        message: "Password reset successful.",
        access_token: userToken("access", "user-1"),
        refresh_token: userToken("refresh", "user-1")
      });
    await completePasswordReset(EMAIL, RESET_CODE, RESET_SECRET, "new-password", CLIENT_ID, {
      mode: "preserve",
      recoveryCode: `${RECOVERY_CODE.toLowerCase().replace(/0/g, "o")}\r\n\t`
    });
    expect(seen[0].body).toEqual({
      proof: PROOF,
      new_password: "new-password",
      mode: { preserve: { recovery_code: RECOVERY_CODE } }
    });
  });

  test("rejects non-code strings before transport and credential installation", async () => {
    for (const recoveryCode of [
      "not a code",
      RECOVERY_CODE.slice(0, -1) + "0",
      RECOVERY_CODE.replace("MPLRC1-0000", "MPLRC1-0001"),
      `Maple recovery code\n\n${RECOVERY_CODE}\n\nKeep this code private.`,
      `${RECOVERY_CODE}\nKeep this code private.`
    ]) {
      await expect(
        completePasswordReset(EMAIL, RESET_CODE, RESET_SECRET, "new-password", CLIENT_ID, {
          mode: "preserve",
          recoveryCode
        })
      ).rejects.toBeInstanceOf(RecoveryCodeFormatError);
    }
    expect(seen).toHaveLength(0);
    expect(readTransportV2Credentials(API_URL, "user")).toBeNull();
  });

  test("a rejected completion keeps its status and installs no credentials", async () => {
    respond = () => jsonResponse({ error: "Bad Request" }, { status: 400 });

    const error = await completePasswordReset(
      EMAIL,
      RESET_CODE,
      RESET_SECRET,
      "new-password",
      CLIENT_ID,
      { mode: "preserve", recoveryCode: RECOVERY_CODE }
    ).catch((caught: unknown) => caught);

    expect(error).toMatchObject({ status: 400 });
    expect(readTransportV2Credentials(API_URL, "user")).toBeNull();
  });

  test("the legacy confirm payload carries no recovery field", async () => {
    await confirmPasswordReset(EMAIL, RESET_CODE, RESET_SECRET, "new-password", CLIENT_ID);

    expect(seen[0].request.target).toBe("/password-reset/confirm");
    expect(seen[0].body).not.toHaveProperty("recovery_code");
  });
});

describe("recovery code management", () => {
  beforeEach(() => {
    installTransportV2Credentials(
      API_URL,
      "user",
      userToken("access", "user-1"),
      userToken("refresh", "user-1")
    );
  });

  test("status is an authenticated GET without a body", async () => {
    const status = {
      enrolled: true,
      issued_at: "2026-11-01T00:00:00Z",
      last_used_at: null,
      use_count: 0
    };
    respond = () => jsonResponse(status);

    expect(await fetchRecoveryCodeStatus()).toEqual(status);
    expect(seen[0].request).toMatchObject({
      method: "GET",
      target: "/protected/recovery-code",
      credential: { kind: "bearer" }
    });
    expect(seen[0].body).toBeUndefined();
  });

  test("enroll and rotate send the current password and return the one-time code", async () => {
    respond = () => jsonResponse({ recovery_code: RECOVERY_CODE });

    expect(await enrollRecoveryCode("current-password")).toEqual({ recovery_code: RECOVERY_CODE });
    expect(await rotateRecoveryCode("current-password")).toEqual({ recovery_code: RECOVERY_CODE });

    expect(seen.map(({ request }) => [request.method, request.target])).toEqual([
      ["POST", "/protected/recovery-code/enroll"],
      ["POST", "/protected/recovery-code/rotate"]
    ]);
    for (const { request, body } of seen) {
      expect(request.credential).toMatchObject({ kind: "bearer" });
      expect(body).toEqual({ current_password: "current-password" });
    }
  });

  test("disable sends a DELETE with the current password", async () => {
    respond = () => jsonResponse({ message: "Recovery disabled successfully" });

    await disableRecoveryCode("current-password");

    expect(seen[0].request).toMatchObject({
      method: "DELETE",
      target: "/protected/recovery-code",
      credential: { kind: "bearer" }
    });
    expect(seen[0].body).toEqual({ current_password: "current-password" });
  });

  test("an existing enrollment surfaces as a 409", async () => {
    respond = () => jsonResponse({ error: "Conflict" }, { status: 409 });

    await expect(enrollRecoveryCode("current-password")).rejects.toMatchObject({ status: 409 });
  });

  test("a different signed-in account is rejected before sending", async () => {
    await expect(enrollRecoveryCode("current-password", "user-2")).rejects.toThrow();
    expect(seen).toHaveLength(0);
  });
});
