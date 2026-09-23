import { getBillingService, type BillingService } from "@/billing/billingService";
import { isTauri } from "@/utils/platform";

export interface BillingPortalDependencies {
  billing: Pick<BillingService, "getPortalUrl" | "captureSessionGuard">;
  native: boolean;
  loadOpener: () => Promise<{ openUrl: (url: string) => Promise<void> }>;
  openWindow: (url: string, target: string, features: string) => unknown;
}

/** Portal links carry account access: never log them or open after owner change. */
export async function openBillingPortal(dependencies?: BillingPortalDependencies): Promise<void> {
  const { billing, native, loadOpener, openWindow } = dependencies ?? {
    billing: getBillingService(),
    native: isTauri(),
    loadOpener: () => import("@tauri-apps/plugin-opener"),
    openWindow: (url: string, target: string, features: string) =>
      window.open(url, target, features)
  };
  const assertCurrent = billing.captureSessionGuard();
  try {
    const url = await billing.getPortalUrl();
    assertCurrent();
    if (native) {
      try {
        const { openUrl } = await loadOpener();
        assertCurrent();
        await openUrl(url);
        assertCurrent();
        return;
      } catch {
        // Preserve the browser fallback, but never for a revoked account.
        assertCurrent();
      }
    }
    assertCurrent();
    // noopener can return null even when the tab opens; do not open a second tab
    // or claim a popup was blocked solely from that return value.
    openWindow(url, "_blank", "noopener,noreferrer");
    assertCurrent();
  } catch {
    assertCurrent();
    throw new Error("Unable to open billing portal");
  }
}
