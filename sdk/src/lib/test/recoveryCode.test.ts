import { describe, expect, test } from "bun:test";
import { normalizeRecoveryCode, RecoveryCodeFormatError } from "../recoveryCode";

const CODE = "MPLRC1-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-39YZ-VTQZ";

describe("recovery code syntax", () => {
  test("canonicalizes standalone codes with case, aliases and ASCII whitespace noise", () => {
    for (const input of [
      CODE,
      CODE.replace(/-/g, ""),
      CODE.toLowerCase().replace(/0/g, "o").replace(/-/g, " \t\r\n\v\f"),
      `${CODE}\n`,
      `${CODE}\r\n`,
      `${CODE}\t`
    ]) {
      expect(normalizeRecoveryCode(input)).toBe(CODE);
    }
  });

  test("rejects prose, file bodies, invalid characters and wrong lengths without echoing input", () => {
    for (const input of [
      "not a recovery code",
      `Maple recovery code\n\n${CODE}`,
      `${CODE}\n\nKeep this code private.`,
      `Maple recovery code\n\n${CODE}\n\nKeep this code private. Replacing or disabling it makes this copy invalid.\n`,
      `${CODE}0`,
      CODE.slice(0, -1),
      CODE.replace("39YZ", "39YU"),
      CODE.replace("39YZ", "39Yſ"),
      `${CODE}\u00a0`,
      `${CODE}\0`,
      `${" ".repeat(256)}${CODE}`
    ]) {
      expect(() => normalizeRecoveryCode(input)).toThrow(RecoveryCodeFormatError);
      expect(() => normalizeRecoveryCode(input)).toThrow("Invalid recovery code format.");
    }
  });

  test("Crockford I and L aliases normalize only in the payload", () => {
    expect(normalizeRecoveryCode(`MPLRC1${"iL".repeat(30)}`)).toBe(`MPLRC1${"-1111".repeat(15)}`);
    expect(() => normalizeRecoveryCode(CODE.replace("MPLRC1", "MP1RC1"))).toThrow(
      RecoveryCodeFormatError
    );
  });
});
