import {
  RECOVERY_CODE_PREFIX,
  RECOVERY_CODE_NORMALIZED_LENGTH,
  RecoveryCodeFormatError,
  stripRecoveryCodeSeparators,
  validateRecoveryCode
} from "@mapleai/sdk";

/** File/prose extraction belongs only to the client; the SDK validates the bounded code. */
export async function extractRecoveryCode(pasted: string): Promise<string> {
  const compact = stripRecoveryCodeSeparators(pasted);
  const start = compact.search(new RegExp(RECOVERY_CODE_PREFIX, "i"));
  if (start < 0) throw new RecoveryCodeFormatError();
  return validateRecoveryCode(compact.slice(start, start + RECOVERY_CODE_NORMALIZED_LENGTH));
}
