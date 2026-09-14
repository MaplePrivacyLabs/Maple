import { afterEach, describe, expect, test } from "bun:test";
import {
  ACCOUNT_CREDENTIAL_MISMATCH_CODE,
  getAuthenticatedUserId,
  assertExpectedAccountPrincipal,
  clearUserCredentialsForSignOut,
  guardAccountResponse
} from "../credentialIdentity";
import {
  clearTransportV2Credentials,
  installTransportV2Credentials,
  readTransportV2Credentials
} from "../transportV2/auth";

const apiUrl = "https://queue-identity.example.test";
const otherUrl = "https://other-queue-identity.example.test";
function token(principal: string, purpose: "access" | "refresh", version = 0): string {
  const encode = (value: unknown) => Buffer.from(JSON.stringify(value)).toString("base64url");
  return [
    encode({ alg: "ES256K", typ: "JWT" }),
    encode({
      aud: `urn:opensecret:internal:transport-v2:user:${purpose}-token`,
      sub: principal,
      tf: 2,
      exp: 4_000_000_000 + version
    }),
    Buffer.from(new Uint8Array(64).fill(1)).toString("base64url")
  ].join(".");
}
function install(principal: string, url = apiUrl, version = 0) {
  return installTransportV2Credentials(
    url,
    "user",
    token(principal, "access", version),
    token(principal, "refresh", version)
  );
}
afterEach(() => {
  clearTransportV2Credentials(apiUrl);
  clearTransportV2Credentials(otherUrl);
  globalThis.localStorage.clear();
});

describe("queue identity with real V2 credential storage", () => {
  test("uses the origin-scoped V2 principal with legacy slots empty", () => {
    install("user-a");
    install("user-b", otherUrl);
    expect(globalThis.localStorage.getItem("access_token")).toBeNull();
    expect(getAuthenticatedUserId(apiUrl)).toBe("user-a");
    expect(getAuthenticatedUserId(otherUrl)).toBe("user-b");
    expect(getAuthenticatedUserId(`${apiUrl}/gateway`)).toBe("user-a");
  });
  test("a same-principal refresh stays valid; replacement and sign-out fail closed", () => {
    install("user-a");
    install("user-a", apiUrl, 1);
    expect(() =>
      assertExpectedAccountPrincipal("user-a", getAuthenticatedUserId(apiUrl))
    ).not.toThrow();
    install("user-b");
    expect(() =>
      assertExpectedAccountPrincipal("user-a", getAuthenticatedUserId(apiUrl))
    ).toThrow();
    clearTransportV2Credentials(apiUrl);
    expect(() =>
      assertExpectedAccountPrincipal("user-a", getAuthenticatedUserId(apiUrl))
    ).toThrow();
  });
  test("an old provider cannot log out a replacement account", () => {
    install("user-a");
    const replacement = install("user-b");
    expect(() => clearUserCredentialsForSignOut(apiUrl, "user-a")).toThrow();
    expect(readTransportV2Credentials(apiUrl, "user")).toEqual(replacement);
  });
  test("logout clears local credentials before remote revocation can await", () => {
    const stored = install("user-a");
    expect(clearUserCredentialsForSignOut(apiUrl, "user-a")).toBe(stored.refreshToken);
    expect(getAuthenticatedUserId(apiUrl)).toBeNull();
    install("user-b");
    expect(getAuthenticatedUserId(apiUrl)).toBe("user-b");
  });
  test("legacy credentials alone cannot authorize a queued operation", () => {
    clearTransportV2Credentials(apiUrl);
    globalThis.localStorage.setItem("access_token", token("user-a", "access"));
    expect(getAuthenticatedUserId(apiUrl)).toBeNull();
  });
  test("stream reads allow token refresh but stop immediately at account replacement", async () => {
    install("user-a");
    let controller!: ReadableStreamDefaultController<Uint8Array>;
    let cancelled = false;
    const source = new Response(
      new ReadableStream<Uint8Array>({
        start(value) {
          controller = value;
        },
        cancel() {
          cancelled = true;
        }
      })
    );
    const guarded = guardAccountResponse(source, () =>
      assertExpectedAccountPrincipal("user-a", getAuthenticatedUserId(apiUrl))
    );
    const reader = guarded.body!.getReader();
    install("user-a", apiUrl, 1);
    controller.enqueue(new Uint8Array([0, 255, 128]));
    expect((await reader.read()).value).toEqual(new Uint8Array([0, 255, 128]));
    const pending = reader.read();
    install("user-b");
    controller.enqueue(new Uint8Array([42]));
    await expect(pending).rejects.toMatchObject({ code: ACCOUNT_CREDENTIAL_MISMATCH_CODE });
    expect(cancelled).toBe(true);
  });
});
