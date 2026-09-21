import assert from "node:assert/strict";
import { StrictMode, type ComponentType, type ReactNode } from "react";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { OpenSecretProvider } from "@mapleai/sdk";
import { AuthSite } from "../AuthSite";
import { markTransportV2DesktopOAuth } from "@/services/desktopOAuthTransport";

class MemoryStorage implements Storage {
  private readonly values = new Map<string, string>();
  get length(): number {
    return this.values.size;
  }
  clear(): void {
    this.values.clear();
  }
  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }
  key(index: number): string | null {
    return [...this.values.keys()][index] ?? null;
  }
  removeItem(key: string): void {
    this.values.delete(key);
  }
  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }
}

const scenario = process.argv[2];
assert.ok(
  ["cold-start", "cold-callback", "retained-start", "retained-callback"].includes(scenario)
);
const retained = scenario.startsWith("retained-");
const callback = scenario.endsWith("-callback");
const apiUrl = "https://bootstrap-api.example.test";
const authOrigin = "https://auth.example.test";
const target = {
  provider: "github" as const,
  nativeSessionId: "00112233445566778899aabbccddeeff",
  nativeRequestId: "ffeeddccbbaa99887766554433221100"
};
const params = new URLSearchParams({
  provider: target.provider,
  transport: "v2",
  native_session_id: target.nativeSessionId,
  native_request_id: target.nativeRequestId
});
const location = new URL(
  callback
    ? `${authOrigin}/auth/github/callback?code=fixture-code&state=fixture-state`
    : `${authOrigin}/start?${params}`
);
const localStorage = new MemoryStorage();
const sessionStorage = new MemoryStorage();
for (const [name, value] of Object.entries({
  localStorage,
  sessionStorage,
  window: { localStorage, sessionStorage, location }
})) {
  Object.defineProperty(globalThis, name, { configurable: true, value });
}

const encode = (value: string) => Buffer.from(value).toString("base64url");
const credentialsKey = `opensecret:transport-v2:auth:v1:${encode(apiUrl)}`;
const continuationKey = `opensecret:transport-v2:oauth-session:v1:${encode(`${apiUrl}\ngithub`)}`;
const pendingKey = "maple_desktop_oauth_pending_v2";
if (retained) {
  const token = (purpose: "access" | "refresh") =>
    `${encode('{"alg":"ES256K"}')}.${encode(
      JSON.stringify({
        aud: `urn:opensecret:internal:transport-v2:user:${purpose}-token`,
        sub: "bootstrap-fixture-user",
        exp: Math.floor(Date.now() / 1000) + 3_600,
        tf: 2
      })
    )}.${encode("synthetic-test-signature")}`;
  localStorage.setItem(
    credentialsKey,
    JSON.stringify({
      version: 1,
      api_origin: apiUrl,
      cache_namespace_root: null,
      user: {
        revision: 1,
        credentials: { access_token: token("access"), refresh_token: token("refresh") }
      },
      platform: { revision: 0, credentials: null }
    })
  );
}
const originalCredentials = localStorage.getItem(credentialsKey);
if (callback) {
  markTransportV2DesktopOAuth(target);
  // The real SDK consumes this only after resolving its configured API origin.
  // It then rejects the deliberately invalid continuation without exchanging a
  // provider code or weakening the attested transport for this regression test.
  sessionStorage.setItem(continuationKey, "invalid test continuation");
}

const requestedUrls: string[] = [];
let releaseBootstrap!: () => void;
const bootstrapResponse = new Promise<Response>((resolve) => {
  releaseBootstrap = () => resolve(new Response("fixture unavailable", { status: 503 }));
});
globalThis.fetch = (async (input: RequestInfo | URL) => {
  const url = input instanceof Request ? input.url : input.toString();
  assert.equal(new URL(url).origin, apiUrl, "SDK must initialize its API origin before use");
  requestedUrls.push(url);
  if (retained && requestedUrls.length === 1) return bootstrapResponse;
  return new Response("fixture unavailable", { status: 503 });
}) as typeof fetch;
// Expected SDK failures contain only synthetic fixtures; silence them so the
// parent test exposes assertion failures rather than normal rejection logging.
console.error = () => {};

// The linked SDK declarations use React 19; runtime is the frontend's React 18
// peer, shared by the same test preload used throughout the frontend suite.
const Provider = OpenSecretProvider as unknown as ComponentType<{
  apiUrl: string;
  clientId: string;
  pcrConfig: { environment: "development" };
  children: ReactNode;
}>;
let renderer: ReactTestRenderer | undefined;
try {
  await act(async () => {
    renderer = create(
      <StrictMode>
        <Provider
          apiUrl={apiUrl}
          clientId="ba5a14b5-d915-47b1-b7b1-afda52bc5fc6"
          pcrConfig={{ environment: "development" }}
        >
          <AuthSite />
        </Provider>
      </StrictMode>
    );
  });
  if (retained) {
    assert.equal(requestedUrls.length, 1, "only retained-session bootstrap may run while pending");
    assert.ok(JSON.stringify(renderer!.toJSON()).includes("Preparing sign-in"));
    if (callback) {
      assert.equal(sessionStorage.getItem(continuationKey), "invalid test continuation");
    } else {
      assert.equal(sessionStorage.getItem(pendingKey), null, "native attempt must not start yet");
    }
    await act(async () => releaseBootstrap());
  }
  if (callback) {
    assert.equal(
      sessionStorage.getItem(continuationKey),
      null,
      "configured callback must reach the SDK continuation boundary"
    );
    assert.ok(JSON.stringify(renderer!.toJSON()).includes("could not be completed"));
  } else {
    assert.ok(sessionStorage.getItem(pendingKey), "native attempt starts after bootstrap");
    assert.equal(
      requestedUrls.length,
      retained ? 2 : 1,
      "OAuth initiation reaches the configured transport"
    );
  }
  assert.equal(
    localStorage.getItem(credentialsKey),
    originalCredentials,
    "bootstrap failures must retain the existing credentials"
  );
} finally {
  await act(async () => renderer?.unmount());
}
console.log("bootstrap verified");
