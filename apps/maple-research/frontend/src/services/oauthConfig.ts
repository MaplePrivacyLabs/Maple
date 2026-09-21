type BrowserOAuthProvider = "github" | "google" | "apple";

const DEFAULT_NATIVE_OAUTH_ENTRY = "https://trymaple.ai/desktop-auth";
const LOOPBACK_HOSTS = new Set(["localhost", "127.0.0.1", "[::1]"]);

function parseOAuthOrigin(value: string, allowLoopbackHttp: boolean): URL {
  // Check the original spelling as well as the parsed URL: URL parsing silently
  // normalizes paths, backslashes, whitespace, and abbreviated IPv4 addresses.
  const match = /^(https?):\/\/(\[[0-9a-fA-F:]+\]|[^\s/?#:@\\]+)(?::[0-9]+)?\/?$/u.exec(value);
  if (!match) throw new Error("Authentication origin must be an HTTPS origin without a path");

  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new Error("Authentication origin is invalid");
  }
  if (
    url.username ||
    url.password ||
    url.pathname !== "/" ||
    url.search ||
    url.hash ||
    (url.protocol !== "https:" &&
      !(
        allowLoopbackHttp &&
        url.protocol === "http:" &&
        LOOPBACK_HOSTS.has(match[2]) &&
        url.hostname === match[2]
      ))
  ) {
    throw new Error("Authentication origin must use HTTPS, or exact loopback HTTP in development");
  }
  return url;
}

/** Browser OAuth always returns to the origin that owns its pending SDK session. */
export function getBrowserOAuthCallbackUrl(provider: BrowserOAuthProvider, origin: string): string {
  if (provider !== "github" && provider !== "google" && provider !== "apple") {
    throw new Error("Unsupported authentication provider");
  }
  return new URL(`/auth/${provider}/callback`, parseOAuthOrigin(origin, true)).toString();
}

/** Direct auth entry remains opt-in until the auth cutover has passed its rollout gate. */
export function getNativeOAuthEntryUrl(configuredOrigin?: string, isDevelopment = false): string {
  if (configuredOrigin === undefined || configuredOrigin === "") return DEFAULT_NATIVE_OAUTH_ENTRY;
  // The permanent alias works on both the original apex page and the auth site.
  return new URL("/desktop-auth", parseOAuthOrigin(configuredOrigin, isDevelopment)).toString();
}
