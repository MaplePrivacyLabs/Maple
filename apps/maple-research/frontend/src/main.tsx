import { StrictMode, type ComponentType } from "react";
import { createRoot } from "react-dom/client";
import { isIOS, isTauri, isTauriDesktop, waitForPlatform } from "@/utils/platform";
import { restoreChatTypographyAtLaunch } from "@/services/chatTypographyPreferences";
import { restoreWorkspaceModeAtLaunch } from "@/services/workspaceModePreference";
import {
  clearRetiredDesktopOAuthMarkers,
  isRetiredDesktopOAuthEntry
} from "@/services/desktopOAuthTransport";
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
    const { ensureNativeTransportRoot } = await import("@/services/nativeTransportRoot");
    await ensureNativeTransportRoot(openSecretClientConfig().apiUrl);
  }
  restoreWorkspaceModeAtLaunch(isTauriDesktop());

  // Markers left by a pre-V2 hosted sign-in must not outlive that flow.
  clearRetiredDesktopOAuthMarkers();

  // Create the router only after restoring the launch route so its first
  // location snapshot matches the user's saved mode. A release from before
  // Transport V2 gets an update notice instead of the application.
  let App: ComponentType;
  if (isRetiredDesktopOAuthEntry(window.location)) {
    await import("./index.css");
    App = (await import("./components/DesktopSignInUpdateRequired")).DesktopSignInUpdateRequired;
  } else {
    App = (await import("./app")).default;
  }

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
