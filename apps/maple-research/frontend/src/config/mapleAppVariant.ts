export type MapleAppVariant = "production" | "dev";

const MAPLE_DEV_API_ORIGIN = "https://enclave.secretgpt.ai";

export function parseMapleAppVariant(value: unknown): MapleAppVariant {
  if (value === undefined || value === "production") return "production";
  if (value === "dev") return "dev";
  throw new Error("Maple app variant is invalid");
}

export function mapleAppVariant(): MapleAppVariant {
  return parseMapleAppVariant(import.meta.env.VITE_MAPLE_APP_VARIANT);
}

export function nativeCallbackScheme(variant: MapleAppVariant): string {
  return variant === "dev" ? "cloud.opensecret.maple.dev" : "cloud.opensecret.maple";
}

export function parseDevAuthOrigin(value: unknown): string {
  if (typeof value === "string") {
    try {
      const url = new URL(value);
      if (url.protocol === "https:" && url.origin === value && url.port === "") return value;
    } catch {
      // Build configuration is public, but never reflect malformed values into errors.
    }
  }
  throw new Error("Maple Dev requires a configured HTTPS sign-in origin");
}

export function nativeAuthOrigin(
  variant: MapleAppVariant,
  devOrigin: unknown = import.meta.env.VITE_MAPLE_DEV_AUTH_ORIGIN
): string {
  return variant === "dev" ? parseDevAuthOrigin(devOrigin) : "https://trymaple.ai";
}

/** The hosted dev handoff is allowed only on the fixed dev site and backend. */
export function hostedNativeAppVariant(
  value: unknown,
  origin: string,
  apiOrigin: string,
  devOrigin: unknown = import.meta.env.VITE_MAPLE_DEV_AUTH_ORIGIN
): MapleAppVariant {
  const variant = parseMapleAppVariant(value);
  if (
    variant === "dev" &&
    (origin !== parseDevAuthOrigin(devOrigin) || apiOrigin !== MAPLE_DEV_API_ORIGIN)
  ) {
    throw new Error("Maple Dev sign-in requires the development site");
  }
  return variant;
}
