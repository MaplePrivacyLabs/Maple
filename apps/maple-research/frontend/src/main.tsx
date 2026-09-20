import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { isIOS, isTauri, isTauriDesktop, waitForPlatform } from "@/utils/platform";
import { restoreChatTypographyAtLaunch } from "@/services/chatTypographyPreferences";
import { restoreWorkspaceModeAtLaunch } from "@/services/workspaceModePreference";
import { shouldLoadLegacyDesktopOAuth } from "@/services/desktopOAuthTransport";
import { openSecretClientConfig } from "@/config/openSecretClientConfig";

// Initialize platform detection before rendering
async function initializeApp() {
  // Restore transcript typography before any asynchronous platform work so a
  // refreshed conversation does not briefly render with the default metrics.
  restoreChatTypographyAtLaunch();

  // Wait for platform detection to complete
  // This ensures all platform checks are correct from the first render
  await waitForPlatform();
  // Explicit local fixture build. The native guard excludes release builds and
  // physical devices even if this public build-time flag is accidentally set.
  if (import.meta.env.VITE_STOREKIT_EXPERIMENT === "1" && isIOS()) {
    const { invoke } = await import("@tauri-apps/api/core");
    if (await invoke<boolean>("storekit_experiment_enabled")) {
      const { default: StoreKitLab } = await import("@/components/dev/StoreKitLab");
      createRoot(document.getElementById("root")!).render(<StoreKitLab />);
      return;
    }
  }
  if (isTauri()) {
    // Keep the V2 SDK out of the hosted V1 compatibility entrypoint. Released
    // callbacks select their pinned bundle before any V2 application module is
    // imported.
    const { ensureNativeTransportRoot } = await import("@/services/nativeTransportRoot");
    await ensureNativeTransportRoot(openSecretClientConfig().apiUrl);
  }
  restoreWorkspaceModeAtLaunch(isTauriDesktop());

  // Create the router only after restoring the launch route so its first
  // location snapshot matches the user's saved mode.
  const { default: App } = shouldLoadLegacyDesktopOAuth(window.location)
    ? await import("./legacy/LegacyDesktopOAuthApp")
    : await import("./app");

  // Render the app
  const rootElement = document.getElementById("root")!;
  if (!rootElement.innerHTML) {
    const root = createRoot(rootElement);
    root.render(
      <StrictMode>
        <App />
      </StrictMode>
    );
  }
}

// Start the app
initializeApp();
