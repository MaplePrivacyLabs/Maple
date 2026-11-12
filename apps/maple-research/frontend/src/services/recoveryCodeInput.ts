import {
  RECOVERY_CODE_PREFIX,
  RECOVERY_CODE_NORMALIZED_LENGTH,
  RecoveryCodeFormatError,
  stripRecoveryCodeSeparators,
  validateRecoveryCode
} from "@mapleai/sdk";

export class AmbiguousRecoveryCodeError extends RecoveryCodeFormatError {
  constructor() {
    super();
    this.name = "AmbiguousRecoveryCodeError";
    this.message =
      "More than one valid recovery code was found. Paste just the code you want to use.";
  }
}

/** File/prose extraction belongs only to the client; the SDK validates the bounded code. */
export async function extractRecoveryCode(pasted: string): Promise<string> {
  const compact = stripRecoveryCodeSeparators(pasted);
  let code: string | undefined;
  for (const match of compact.matchAll(new RegExp(RECOVERY_CODE_PREFIX, "gi"))) {
    let candidate: string;
    try {
      candidate = await validateRecoveryCode(
        compact.slice(match.index, match.index + RECOVERY_CODE_NORMALIZED_LENGTH)
      );
    } catch (failure) {
      if (failure instanceof RecoveryCodeFormatError) continue;
      throw failure;
    }
    if (code && code !== candidate) throw new AmbiguousRecoveryCodeError();
    code = candidate;
  }
  if (!code) throw new RecoveryCodeFormatError();
  return code;
}
