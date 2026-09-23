import { storeKit } from "@/services/storeKitService";
import { isIOS, isTauri } from "@/utils/platform";
import { APPLE_SUBSCRIPTIONS_URL } from "./subscriptionManagement";

type ManagementDependencies = {
  isIOS: () => boolean;
  isTauri: () => boolean;
  manageNative: () => Promise<void>;
  openNativeUrl: (url: string) => Promise<void>;
  openWebUrl: (url: string) => void;
};

const defaults: ManagementDependencies = {
  isIOS,
  isTauri,
  manageNative: () => storeKit.manageSubscriptions(),
  openNativeUrl: async (url) => {
    const { openUrl } = await import("@tauri-apps/plugin-opener");
    await openUrl(url);
  },
  openWebUrl: (url) => {
    window.open(url, "_blank", "noopener,noreferrer");
  }
};

/** Always available independently of the new-purchase kill switch. */
export async function openAppleSubscriptionManagement(
  dependencies: ManagementDependencies = defaults
): Promise<void> {
  if (dependencies.isIOS()) {
    await dependencies.manageNative();
  } else if (dependencies.isTauri()) {
    await dependencies.openNativeUrl(APPLE_SUBSCRIPTIONS_URL);
  } else {
    dependencies.openWebUrl(APPLE_SUBSCRIPTIONS_URL);
  }
}
