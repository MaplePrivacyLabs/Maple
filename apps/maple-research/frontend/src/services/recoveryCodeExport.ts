import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { normalizeRecoveryCode } from "@mapleai/sdk";
import { isTauriDesktop, isTauriMobile } from "@/utils/platform";

type SaveResult = "saved" | "cancelled" | "downloaded";
type Invoke = (command: string, args: { recoveryCode: string }) => Promise<boolean>;

export function createRecoveryCodeSaver(
  desktop = isTauriDesktop,
  mobile = isTauriMobile,
  invoke: Invoke = tauriInvoke
) {
  return async (code: string): Promise<SaveResult> => {
    const recoveryCode = normalizeRecoveryCode(code);
    if (desktop()) {
      return (await invoke("save_recovery_code", { recoveryCode })) ? "saved" : "cancelled";
    }
    if (mobile()) throw new Error("File export is unavailable on this platform");

    const url = URL.createObjectURL(
      new Blob(
        [
          `Maple recovery code\n\n${recoveryCode}\n\nKeep this code private. Replacing or disabling it makes this copy invalid.\n`
        ],
        { type: "text/plain;charset=utf-8" }
      )
    );
    const link = document.createElement("a");
    try {
      link.href = url;
      link.download = "maple-recovery-code.txt";
      document.body.appendChild(link);
      link.click();
    } finally {
      link.remove();
      // Let the browser consume the Blob before releasing its URL.
      setTimeout(() => URL.revokeObjectURL(url), 1000);
    }
    return "downloaded";
  };
}

export const saveRecoveryCode = createRecoveryCodeSaver();
