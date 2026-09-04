import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";
import { useNotification } from "@/contexts/NotificationContext";
import { Server } from "lucide-react";
import { isTauri } from "@/utils/platform";
import { proxyService } from "@/services/proxyService";

export function ProxyEventListener() {
  const { showNotification } = useNotification();

  useEffect(() => {
    // Only setup listeners if running on Tauri (not web)
    if (!isTauri()) {
      return;
    }

    let unlistenAutoStarted: (() => void) | null = null;
    let unlistenAutoStartFailed: (() => void) | null = null;
    let disposed = false;

    const setupListeners = async () => {
      try {
        // Listen for proxy auto-start success
        const stopAutoStarted = await listen("proxy-autostarted", (event) => {
          const config = event.payload as { host: string; port: number };
          showNotification({
            type: "success",
            title: "Proxy Started",
            message: `Local proxy is running on ${config.host}:${config.port}`,
            icon: <Server className="h-5 w-5 text-maple-success" />,
            duration: 5000
          });
        });
        if (disposed) {
          stopAutoStarted();
          return;
        }
        unlistenAutoStarted = stopAutoStarted;

        // Listen for proxy auto-start failure
        const stopAutoStartFailed = await listen("proxy-autostart-failed", (event) => {
          const error = event.payload as string;
          showNotification({
            type: "error",
            title: "Proxy Auto-Start Failed",
            message: error || "Failed to start the proxy automatically",
            duration: 7000
          });
        });
        if (disposed) {
          stopAutoStartFailed();
          return;
        }
        unlistenAutoStartFailed = stopAutoStartFailed;

        // The native command runs only after both listeners exist and after
        // the stable Transport V2 root for the saved backend is installed.
        await proxyService.initializeProxyOnStartup();
      } catch (error) {
        if (!disposed) {
          console.error("Failed to setup proxy event listeners:", error);
        }
      }
    };

    setupListeners();

    // Cleanup listeners on unmount
    return () => {
      disposed = true;
      if (unlistenAutoStarted) unlistenAutoStarted();
      if (unlistenAutoStartFailed) unlistenAutoStartFailed();
    };
  }, [showNotification]);

  return null; // This component doesn't render anything
}
