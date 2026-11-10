import { describe, expect, test } from "bun:test";
import { RecoveryCodeFormatError } from "@mapleai/sdk";
import { AmbiguousRecoveryCodeError, extractRecoveryCode } from "./recoveryCodeInput";

const code = "MPLRC1-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-39YZ-VTQZ";
const other = "MPLRC1-000G-40R4-0M30-E209-185G-R38E-1W81-24GK-2GAH-C5RR-34D1-P70X-3RFG-948Q-DC7M";

describe("extractRecoveryCode", () => {
  test("skips a prefix in prose and an invalid checksum before a valid saved code", async () => {
    await expect(
      extractRecoveryCode(
        `The MPLRC1 format\nOld: ${code.slice(0, -1)}0\nSaved: ${other}\nKeep private.`
      )
    ).resolves.toBe(other);
  });

  test("deduplicates repeated codes after alias, case, and separator normalization", async () => {
    await expect(
      extractRecoveryCode(
        `${code}\nCopy: ${code.toLowerCase().replace(/0/g, "o")}\n${code.replace(/-/g, "\t")}`
      )
    ).resolves.toBe(code);
  });

  test("rejects distinct valid codes in either order", async () => {
    await expect(extractRecoveryCode(`${code}\n${other}`)).rejects.toBeInstanceOf(
      AmbiguousRecoveryCodeError
    );
    await expect(extractRecoveryCode(`${other}\n${code}`)).rejects.toBeInstanceOf(
      AmbiguousRecoveryCodeError
    );
  });

  test("rejects a paste with no valid candidate", async () => {
    await expect(
      extractRecoveryCode(`MPLRC1 format\n${code.slice(0, -1)}0`)
    ).rejects.toBeInstanceOf(RecoveryCodeFormatError);
  });
});
