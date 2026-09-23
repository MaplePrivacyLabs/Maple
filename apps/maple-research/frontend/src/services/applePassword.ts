import { invoke } from "@tauri-apps/api/core";

const GENERIC_FAILURE = "Something went wrong. Please try again.";
const MISSING_ENTRY = "No Apple Passwords entry for trymaple.ai.";
const NOT_AN_EMAIL = "Apple Passwords did not return an email for trymaple.ai.";

export type ApplePasswordSheet =
  | { status: "selected"; username: string; password: string }
  | { status: "cancelled" }
  | { status: "unavailable" }
  | { status: "failed"; message: string };

export type ApplePasswordSignIn =
  | { status: "signed-in" }
  | { status: "cancelled" }
  | { status: "unavailable"; message: string }
  | { status: "failed"; message: string };

type ApplePasswordRequester = () => Promise<unknown>;

function failed(message: string): ApplePasswordSheet {
  return { status: "failed", message };
}

export function parseApplePasswordSheet(value: unknown): ApplePasswordSheet {
  if (typeof value !== "object" || value === null) return failed(GENERIC_FAILURE);
  const record = value as Record<string, unknown>;
  if (record.status === "cancelled") return { status: "cancelled" };
  if (record.status === "unavailable") return { status: "unavailable" };
  if (record.status === "failed") {
    if (
      typeof record.message !== "string" ||
      record.message.length === 0 ||
      record.message.length > 300
    ) {
      return failed(GENERIC_FAILURE);
    }
    return { status: "failed", message: record.message };
  }
  if (
    record.status === "selected" &&
    typeof record.username === "string" &&
    typeof record.password === "string"
  ) {
    return { status: "selected", username: record.username, password: record.password };
  }
  return failed(GENERIC_FAILURE);
}

export function isApplePasswordEmail(username: string): boolean {
  const email = username.trim();
  return email.length > 0 && email.length <= 320 && email.includes("@") && !/\s/u.test(email);
}

function signInFailureMessage(error: unknown, password: string): string {
  if (!(error instanceof Error) || error.message.length === 0 || error.message.includes(password)) {
    return GENERIC_FAILURE;
  }
  return error.message;
}

export async function requestApplePassword(
  request: ApplePasswordRequester = () => invoke("request_apple_password")
): Promise<ApplePasswordSheet> {
  try {
    return parseApplePasswordSheet(await request());
  } catch {
    return failed(GENERIC_FAILURE);
  }
}

export async function signInWithApplePassword(options: {
  request?: ApplePasswordRequester;
  signIn: (email: string, password: string) => Promise<void>;
}): Promise<ApplePasswordSignIn> {
  const sheet = await requestApplePassword(options.request);
  if (sheet.status === "cancelled") return { status: "cancelled" };
  if (sheet.status === "unavailable") return { status: "unavailable", message: MISSING_ENTRY };
  if (sheet.status === "failed") return sheet;
  const email = sheet.username.trim();
  if (!isApplePasswordEmail(email) || sheet.password.length === 0) {
    return { status: "unavailable", message: NOT_AN_EMAIL };
  }
  try {
    await options.signIn(email, sheet.password);
    return { status: "signed-in" };
  } catch (error) {
    return { status: "failed", message: signInFailureMessage(error, sheet.password) };
  }
}
