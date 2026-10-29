import { APP_ORIGIN } from "@/config/domains";
import { isMobile, isTauri } from "@/utils/platform";

/** Browser returns stay with their tab; native returns need an HTTPS website. */
export function billingReturnOrigin(): string {
  if (!isTauri()) return window.location.origin;

  // Keep the existing Universal/App Link host for mobile clients.
  if (isMobile()) return "https://trymaple.ai";

  return nativeAppOrigin();
}

/** Portal exits stay on the website and do not need a mobile payment deep link. */
export function billingPortalReturnOrigin(): string {
  return isTauri() ? nativeAppOrigin() : window.location.origin;
}

function nativeAppOrigin(): string {
  // PR builds can use hosted Dev billing without the mobile Dev app variant.
  return import.meta.env.VITE_MAPLE_BILLING_API_URL?.replace(/\/+$/, "") ===
    "https://billing-dev.opensecret.cloud"
    ? "https://app-dev.maple.ai"
    : APP_ORIGIN;
}
