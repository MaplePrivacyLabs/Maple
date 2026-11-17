import { describe, expect, mock, test } from "bun:test";
import { RECOVERY_CODE_PAYLOAD_LENGTH, normalizeRecoveryCode } from "@mapleai/sdk";
import { createRecoveryCodeSaver } from "./recoveryCodeExport";

const code = normalizeRecoveryCode(`MPLRC1${"0".repeat(RECOVERY_CODE_PAYLOAD_LENGTH)}`);

describe("recovery code export", () => {
  test("desktop invokes the native saver with only the normalized code", async () => {
    const invoke = mock(async () => true);
    const save = createRecoveryCodeSaver(
      () => true,
      () => false,
      invoke
    );
    expect(await save(code.toLowerCase())).toBe("saved");
    expect(invoke).toHaveBeenCalledWith("save_recovery_code", { recoveryCode: code });
  });

  test("cancelling the native dialog is distinct from saving or failing", async () => {
    const save = createRecoveryCodeSaver(
      () => true,
      () => false,
      async () => false
    );
    expect(await save(code)).toBe("cancelled");
  });

  test("propagates native failures and rejects malformed input before invoking native code", async () => {
    const invoke = mock(async () => {
      throw new Error("write failed");
    });
    const save = createRecoveryCodeSaver(
      () => true,
      () => false,
      invoke
    );
    await expect(save(code)).rejects.toThrow("write failed");
    invoke.mockClear();
    await expect(save("prose instead of a code")).rejects.toThrow("Invalid recovery code format");
    expect(invoke).not.toHaveBeenCalled();
  });

  test("mobile reports unsupported export instead of pretending a download succeeded", async () => {
    const invoke = mock(async () => true);
    const save = createRecoveryCodeSaver(
      () => false,
      () => true,
      invoke
    );
    await expect(save(code)).rejects.toThrow("unavailable");
    expect(invoke).not.toHaveBeenCalled();
  });
});
