import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { encodeURLSafe } from "@stablelib/base64";
import type { PcrConfig } from "../../pcr";
import {
  getPlatformApiUrl,
  getPlatformPcrConfig,
  getPushSettings,
  setPlatformApiUrl,
  updatePushSettings,
  type PushSettings
} from "../../platformApi";
import { clearTransportV2Credentials, installTransportV2Credentials } from "../../transportV2/auth";
import {
  transportV2Runtime,
  type TransportV2RuntimeRequest,
  type TransportV2RuntimeResponse
} from "../../transportV2/runtime";

const platformApiUrl = "https://platform.example.com";
const pcrConfig: PcrConfig = { environment: "development", remoteAttestation: false };
const originalPlatformApiUrl = getPlatformApiUrl();
const originalPlatformPcrConfig = getPlatformPcrConfig();
const originalRuntimeRequest = transportV2Runtime.request;

function segment(value: unknown): string {
  return encodeURLSafe(new TextEncoder().encode(JSON.stringify(value))).replace(/=+$/u, "");
}

function token(audience: string): string {
  return `${segment({ alg: "ES256K", typ: "JWT" })}.${segment({
    aud: audience,
    sub: "platform-developer",
    exp: 4_000_000_000,
    tf: 2
  })}.${segment("signature")}`;
}

function exchange(response: Response): TransportV2RuntimeResponse {
  return { response, rememberOAuthContinuation: () => {} };
}

beforeEach(() => {
  globalThis.localStorage.clear();
  globalThis.sessionStorage.clear();
  clearTransportV2Credentials(platformApiUrl);
  transportV2Runtime.clear();
  setPlatformApiUrl(platformApiUrl, pcrConfig);
  installTransportV2Credentials(
    platformApiUrl,
    "platform",
    token("urn:opensecret:internal:transport-v2:platform:access-token"),
    token("urn:opensecret:internal:transport-v2:platform:refresh-token")
  );
});

afterEach(() => {
  transportV2Runtime.request = originalRuntimeRequest;
  transportV2Runtime.clear();
  clearTransportV2Credentials(platformApiUrl);
  setPlatformApiUrl(originalPlatformApiUrl, originalPlatformPcrConfig);
  globalThis.localStorage.clear();
  globalThis.sessionStorage.clear();
});

test("getPushSettings calls the project push settings endpoint through V2", async () => {
  const responseSettings: PushSettings = {
    encrypted_preview_enabled: true,
    ios: {
      enabled: true,
      bundle_id: "ai.trymaple.ios",
      apns_environment: "prod",
      team_id: "TEAM123",
      key_id: "KEY123"
    },
    android: {
      enabled: true,
      firebase_project_id: "firebase-project",
      package_name: "ai.trymaple.android"
    }
  };
  let seen: TransportV2RuntimeRequest | undefined;
  transportV2Runtime.request = mock(async (input) => {
    seen = input;
    return exchange(Response.json(responseSettings));
  });

  await expect(getPushSettings("org-123", "project-456")).resolves.toEqual(responseSettings);
  expect(seen?.request).toMatchObject({
    method: "GET",
    target: "/platform/orgs/org-123/projects/project-456/settings/push",
    body: undefined,
    credential: { kind: "bearer" }
  });
});

test("updatePushSettings puts JSON bytes inside the authenticated V2 request", async () => {
  const requestSettings: PushSettings = {
    encrypted_preview_enabled: true,
    ios: {
      enabled: true,
      bundle_id: "ai.trymaple.ios",
      apns_environment: "dev",
      team_id: "TEAM456",
      key_id: "KEY456"
    },
    android: {
      enabled: false,
      firebase_project_id: "firebase-project",
      package_name: "ai.trymaple.android"
    }
  };
  let seenBody: Uint8Array | undefined;
  let seen: TransportV2RuntimeRequest | undefined;
  transportV2Runtime.request = mock(async (input) => {
    seen = input;
    seenBody = input.request.body ? new Uint8Array(input.request.body) : undefined;
    return exchange(Response.json(requestSettings));
  });

  await expect(updatePushSettings("org-123", "project-456", requestSettings)).resolves.toEqual(
    requestSettings
  );
  expect(seen?.request).toMatchObject({
    method: "PUT",
    target: "/platform/orgs/org-123/projects/project-456/settings/push",
    headers: [{ name: "content-type", value: "application/json" }],
    credential: { kind: "bearer" }
  });
  expect(JSON.parse(new TextDecoder().decode(seenBody))).toEqual(requestSettings);
});
