export const RECOVERY_CODE_PREFIX = "MPLRC1";
export const RECOVERY_CODE_ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
export const RECOVERY_CODE_GROUP_SIZE = 4;
export const RECOVERY_CODE_VERSION = 1;
export const RECOVERY_CODE_SECRET_BYTES = 32;
export const RECOVERY_CODE_CHECKSUM_BYTES = 5;
const SECRET_ENCODED_LENGTH = Math.ceil((RECOVERY_CODE_SECRET_BYTES * 8) / 5);
export const RECOVERY_CODE_PAYLOAD_LENGTH =
  SECRET_ENCODED_LENGTH + Math.ceil((RECOVERY_CODE_CHECKSUM_BYTES * 8) / 5);
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

/** Validates a standalone code, including Crockford padding and its versioned checksum. */
export async function validateRecoveryCode(input: string): Promise<string> {
  const canonical = normalizeRecoveryCode(input);
  const payload = stripRecoveryCodeSeparators(canonical).slice(RECOVERY_CODE_PREFIX.length);
  // The 32-byte secret and 5-byte checksum are encoded separately. The
  // secret's last symbol contains four zero padding bits, not checksum bits.
  const decode = (encoded: string): Uint8Array => {
    const bytes: number[] = [];
    let buffer = 0;
    let bits = 0;
    for (const character of encoded) {
      buffer = (buffer << 5) | RECOVERY_CODE_ALPHABET.indexOf(character);
      bits += 5;
      if (bits >= 8) {
        bits -= 8;
        bytes.push((buffer >>> bits) & 255);
        buffer &= (1 << bits) - 1;
      }
    }
    if (buffer !== 0) throw new RecoveryCodeFormatError();
    return new Uint8Array(bytes);
  };
  const secret = decode(payload.slice(0, SECRET_ENCODED_LENGTH));
  const checksum = decode(payload.slice(SECRET_ENCODED_LENGTH));
  const versionedSecret = new Uint8Array(RECOVERY_CODE_SECRET_BYTES + 1);
  versionedSecret[0] = RECOVERY_CODE_VERSION;
  versionedSecret.set(secret, 1);
  try {
    const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", versionedSecret));
    if (checksum.some((byte, index) => byte !== hash[index])) throw new RecoveryCodeFormatError();
    return canonical;
  } finally {
    secret.fill(0);
    versionedSecret.fill(0);
  }
}
