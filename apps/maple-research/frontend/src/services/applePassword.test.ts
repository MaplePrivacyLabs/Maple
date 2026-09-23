import { describe, expect, test } from "bun:test";
import {
  isApplePasswordEmail,
  parseApplePasswordSheet,
  signInWithApplePassword
} from "./applePassword";

const FIXTURE_PASSWORD = "fixture-password";

describe("parseApplePasswordSheet", () => {
  test("accepts the four command results and rejects anything else", () => {
    expect(parseApplePasswordSheet({ status: "cancelled" })).toEqual({ status: "cancelled" });
    expect(parseApplePasswordSheet({ status: "unavailable" })).toEqual({ status: "unavailable" });
    expect(
      parseApplePasswordSheet({
        status: "selected",
        username: "ada@example.com",
        password: FIXTURE_PASSWORD
      })
    ).toEqual({
      status: "selected",
      username: "ada@example.com",
      password: FIXTURE_PASSWORD
    });
    expect(parseApplePasswordSheet({ status: "failed", message: "closed" })).toEqual({
      status: "failed",
      message: "closed"
    });
    expect(
      parseApplePasswordSheet({ status: "selected", username: "ada@example.com" }).status
    ).toBe("failed");
    expect(parseApplePasswordSheet("cancelled").status).toBe("failed");
  });
});

describe("signInWithApplePassword", () => {
  test("signs in with the trimmed email and does not echo the password on failure", async () => {
    const seen: string[] = [];
    const signedIn = await signInWithApplePassword({
      request: async () => ({
        status: "selected",
        username: " ada@example.com ",
        password: FIXTURE_PASSWORD
      }),
      signIn: async (email, password) => {
        seen.push(`${email}:${password}`);
      }
    });
    expect(signedIn).toEqual({ status: "signed-in" });
    expect(seen).toEqual([`ada@example.com:${FIXTURE_PASSWORD}`]);

    const leaked = await signInWithApplePassword({
      request: async () => ({
        status: "selected",
        username: "ada@example.com",
        password: FIXTURE_PASSWORD
      }),
      signIn: async () => {
        throw new Error(`rejected ${FIXTURE_PASSWORD}`);
      }
    });
    expect(leaked.status).toBe("failed");
    if (leaked.status === "failed") {
      expect(leaked.message).not.toContain(FIXTURE_PASSWORD);
    }
  });

  test("stays quiet on cancel and refuses a non-email username", async () => {
    let calls = 0;
    const cancelled = await signInWithApplePassword({
      request: async () => ({ status: "cancelled" }),
      signIn: async () => {
        calls += 1;
      }
    });
    expect(cancelled).toEqual({ status: "cancelled" });
    expect(calls).toBe(0);

    const guest = await signInWithApplePassword({
      request: async () => ({
        status: "selected",
        username: "account-id",
        password: FIXTURE_PASSWORD
      }),
      signIn: async () => {
        calls += 1;
      }
    });
    expect(guest.status).toBe("unavailable");
    expect(calls).toBe(0);
    expect(isApplePasswordEmail("ada@example.com")).toBe(true);
    expect(isApplePasswordEmail("ada example.com")).toBe(false);
  });
});
