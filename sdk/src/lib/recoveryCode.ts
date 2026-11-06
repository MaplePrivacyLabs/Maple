export const RECOVERY_CODE_PREFIX = "MPLRC1";
export const RECOVERY_CODE_ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
export const RECOVERY_CODE_GROUP_SIZE = 4;
export const RECOVERY_CODE_PAYLOAD_LENGTH = 60;
export const RECOVERY_CODE_NORMALIZED_LENGTH =
  RECOVERY_CODE_PREFIX.length + RECOVERY_CODE_PAYLOAD_LENGTH;
export const RECOVERY_CODE_MAX_INPUT_LENGTH = 256;

/** A local format failure: no request was sent and no reset proof was consumed. */
export class RecoveryCodeFormatError extends Error {
  constructor() {
    super("Invalid recovery code format.");
    this.name = "RecoveryCodeFormatError";
  }
}

/** Character noise only. This does not extract a code from surrounding text. */
export function stripRecoveryCodeSeparators(input: string): string {
  return input.replace(/[\t-\r -]/g, "");
}

/**
 * Accepts a standalone canonical or normalizable code and returns its canonical
 * grouped form. The server verifies its checksum, padding and enrollment.
 * Surrounding prose is invalid; extraction belongs to the calling application.
 */
export function normalizeRecoveryCode(input: string): string {
  if (
    typeof input !== "string" ||
    input.length > RECOVERY_CODE_MAX_INPUT_LENGTH ||
    /[^\x00-\x7f]/.test(input)
  ) {
    throw new RecoveryCodeFormatError();
  }
  const normalized = stripRecoveryCodeSeparators(input).toUpperCase();
  if (
    normalized.length !== RECOVERY_CODE_NORMALIZED_LENGTH ||
    !normalized.startsWith(RECOVERY_CODE_PREFIX)
  ) {
    throw new RecoveryCodeFormatError();
  }
  // Aliases apply only to the payload, not the literal version prefix.
  const payload = normalized
    .slice(RECOVERY_CODE_PREFIX.length)
    .replace(/O/g, "0")
    .replace(/[IL]/g, "1");
  if ([...payload].some((character) => !RECOVERY_CODE_ALPHABET.includes(character))) {
    throw new RecoveryCodeFormatError();
  }
  const groups = [];
  for (let offset = 0; offset < payload.length; offset += RECOVERY_CODE_GROUP_SIZE) {
    groups.push(payload.slice(offset, offset + RECOVERY_CODE_GROUP_SIZE));
  }
  return [RECOVERY_CODE_PREFIX, ...groups].join("-");
}
