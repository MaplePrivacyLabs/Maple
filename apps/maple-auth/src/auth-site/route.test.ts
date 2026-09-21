import { describe, expect, test } from "bun:test";
import { parseAuthSiteRoute } from "./route";

const nativeSessionId = "00112233445566778899aabbccddeeff";
const nativeRequestId = "ffeeddccbbaa99887766554433221100";
const providers = ["github", "google", "apple"] as const;

function startParams(provider = "github"): URLSearchParams {
  return new URLSearchParams({
    transport: "v2",
    provider,
    native_session_id: nativeSessionId,
    native_request_id: nativeRequestId
  });
}

function parse(pathname: string, params?: URLSearchParams) {
  return parseAuthSiteRoute({ pathname, search: params ? `?${params.toString()}` : "" });
}

describe("auth site start routes", () => {
  for (const pathname of ["/start", "/desktop-auth"]) {
    for (const provider of providers) {
      test(`accepts ${provider} V2 initiation through ${pathname}`, () => {
        expect(parse(pathname, startParams(provider))).toEqual({
          kind: "start",
          provider,
          nativeSessionId,
          nativeRequestId
        });
      });
    }
  }

  for (const key of ["transport", "provider", "native_session_id", "native_request_id"]) {
    test(`rejects missing or empty ${key}`, () => {
      const missing = startParams();
      missing.delete(key);
      expect(parse("/start", missing)).toEqual({ kind: "invalid" });

      const empty = startParams();
      empty.set(key, "");
      expect(parse("/start", empty)).toEqual({ kind: "invalid" });
    });

    test(`rejects duplicate ${key} even when both values agree`, () => {
      const params = startParams();
      params.append(key, params.get(key)!);
      expect(parse("/start", params)).toEqual({ kind: "invalid" });
    });
  }

  test("rejects the legacy alias without an explicit V2 target", () => {
    expect(parse("/desktop-auth")).toEqual({ kind: "invalid" });
    expect(parse("/desktop-auth", new URLSearchParams({ provider: "github" }))).toEqual({
      kind: "invalid"
    });

    const params = startParams();
    params.set("transport", "v1");
    expect(parse("/desktop-auth", params)).toEqual({ kind: "invalid" });
  });

  test("does not normalize unsupported transport or provider values", () => {
    for (const transport of ["V2", "v2 ", "v3"]) {
      const params = startParams();
      params.set("transport", transport);
      expect(parse("/start", params)).toEqual({ kind: "invalid" });
    }
    for (const provider of ["GitHub", "google ", "microsoft", "github,google"]) {
      expect(parse("/start", startParams(provider))).toEqual({ kind: "invalid" });
    }
  });

  for (const key of ["native_session_id", "native_request_id"]) {
    test(`requires exactly 32 lowercase hexadecimal characters for ${key}`, () => {
      for (const value of [
        nativeSessionId.toUpperCase(),
        nativeSessionId.slice(1),
        `${nativeSessionId}0`,
        `g${nativeSessionId.slice(1)}`,
        ` ${nativeSessionId}`,
        `${nativeSessionId}\n`,
        "00112233-4455-6677-8899-aabbccddeeff"
      ]) {
        const params = startParams();
        params.set(key, value);
        expect(parse("/start", params)).toEqual({ kind: "invalid" });
      }
    });
  }

  test("rejects extra query fields rather than accepting caller-chosen destinations or credentials", () => {
    for (const [key, value] of [
      ["next", "https://untrusted.example/return"],
      ["redirect_uri", "https://untrusted.example/callback"],
      ["invite_code", "fixture-invite"],
      ["access_token", "fixture-access-token"],
      ["refresh_token", "fixture-refresh-token"],
      ["unexpected", ""]
    ]) {
      const params = startParams();
      params.append(key, value);
      expect(parse("/start", params)).toEqual({ kind: "invalid" });
    }

    const duplicateInvite = startParams();
    duplicateInvite.append("invite_code", "first");
    duplicateInvite.append("invite_code", "second");
    expect(parse("/desktop-auth", duplicateInvite)).toEqual({ kind: "invalid" });
  });

  test("detects duplicate keys after percent decoding", () => {
    expect(
      parseAuthSiteRoute({
        pathname: "/start",
        search: `?${startParams().toString()}&%70rovider=google`
      })
    ).toEqual({ kind: "invalid" });
  });
});

describe("auth site callback routes", () => {
  for (const provider of providers) {
    test(`accepts ${provider} callback values without imposing a state format`, () => {
      const code = "fixture code/+=";
      const state = "opaque:fixture/state+value=";
      expect(parse(`/auth/${provider}/callback`, new URLSearchParams({ code, state }))).toEqual({
        kind: "callback",
        provider,
        code,
        state
      });
    });
  }

  for (const key of ["code", "state"]) {
    test(`rejects missing, empty, or duplicate callback ${key}`, () => {
      const params = new URLSearchParams({ code: "fixture-code", state: "fixture-state" });
      params.delete(key);
      expect(parse("/auth/github/callback", params)).toEqual({ kind: "invalid" });
      params.set(key, "");
      expect(parse("/auth/github/callback", params)).toEqual({ kind: "invalid" });
      params.set(key, "fixture-value");
      params.append(key, "fixture-value");
      expect(parse("/auth/github/callback", params)).toEqual({ kind: "invalid" });
    });
  }

  test("rejects provider errors even when code and state are also present", () => {
    for (const key of ["error", "error_description", "error_uri"]) {
      const params = new URLSearchParams({ code: "fixture-code", state: "fixture-state" });
      params.set(key, "fixture-error");
      expect(parse("/auth/google/callback", params)).toEqual({ kind: "invalid" });
    }
    expect(parse("/auth/google/callback", new URLSearchParams({ error: "access_denied" }))).toEqual(
      { kind: "invalid" }
    );
  });

  test("ignores optional provider metadata without changing the callback identity", () => {
    const params = new URLSearchParams({
      code: "fixture-code",
      state: "fixture-state",
      scope: "openid email profile",
      authuser: "0",
      prompt: "consent",
      provider_metadata: "fixture-extra"
    });
    expect(parse("/auth/google/callback", params)).toEqual({
      kind: "callback",
      provider: "google",
      code: "fixture-code",
      state: "fixture-state"
    });
  });
});

describe("auth site route boundaries", () => {
  test("accepts completion only without query parameters", () => {
    expect(parse("/complete")).toEqual({ kind: "complete" });
    expect(parse("/complete", new URLSearchParams({ next: "/start" }))).toEqual({
      kind: "invalid"
    });
  });

  test("rejects unrelated, ambiguous, and noncanonical route paths", () => {
    for (const pathname of [
      "/",
      "/login",
      "/signup",
      "/pricing",
      "/start/",
      "/desktop-auth/",
      "/complete/",
      "//start",
      "/auth/microsoft/callback",
      "/auth/GitHub/callback",
      "/auth/github/callback/",
      "/auth/github/callback/extra",
      "/auth/github%2fcallback"
    ]) {
      expect(parse(pathname, startParams())).toEqual({ kind: "invalid" });
    }
  });
});
