import { describe, expect, test } from "bun:test";
import {
  normalizeRecoveryCode,
  validateRecoveryCode,
  RecoveryCodeFormatError
} from "../recoveryCode";

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

const OTHER_CODE =
  "MPLRC1-000G-40R4-0M30-E209-185G-R38E-1W81-24GK-2GAH-C5RR-34D1-P70X-3RFG-948Q-DC7M";

describe("recovery code integrity", () => {
  test("accepts backend-compatible checksum vectors and normalizable aliases", async () => {
    for (const code of [CODE, OTHER_CODE]) {
      expect(await validateRecoveryCode(code)).toBe(code);
      const [prefix, ...groups] = code.split("-");
      for (const alias of ["i", "l"]) {
        const noisy =
          prefix.toLowerCase() +
          " " +
          groups.join("\n").toLowerCase().replace(/0/g, "o").replace(/1/g, alias);
        expect(await validateRecoveryCode(noisy)).toBe(code);
      }
    }
  });

  test("rejects a changed secret, changed checksum and nonzero secret padding locally", async () => {
    const compact = CODE.replaceAll("-", "");
    for (const code of [
      CODE.replace("MPLRC1-0000", "MPLRC1-8000"),
      CODE.slice(0, -1) + "0",
      compact.slice(0, 57) + "1" + compact.slice(58),
      "Maple recovery code\n" + CODE
    ]) {
      await expect(validateRecoveryCode(code)).rejects.toBeInstanceOf(RecoveryCodeFormatError);
    }
  });
});
