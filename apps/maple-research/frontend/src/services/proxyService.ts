import { invoke } from "@tauri-apps/api/core";
import { isTauriDesktop } from "@/utils/platform";
import { ensureNativeTransportRoot } from "@/services/nativeTransportRoot";

export interface ProxyConfig {
  host: string;
  port: number;
  api_key: string;
  enabled: boolean;
  enable_cors?: boolean;
  backend_url?: string;
  auto_start?: boolean;
}

export interface ProxyStatus {
  running: boolean;
  config: ProxyConfig;
  error?: string;
}

const DEFAULT_PROXY_BACKEND_URL = "https://enclave.trymaple.ai";

export function manualProxyConfigsMatch(active: ProxyConfig, desired: ProxyConfig): boolean {
  return (
    active.host.trim().toLowerCase() === desired.host.trim().toLowerCase() &&
    active.port === desired.port &&
    active.api_key.trim() === desired.api_key.trim() &&
    active.enabled === desired.enabled &&
    (active.enable_cors ?? false) === (desired.enable_cors ?? false) &&
    normalizeBackendUrl(active.backend_url) === normalizeBackendUrl(desired.backend_url) &&
    (active.auto_start ?? false) === (desired.auto_start ?? false)
  );
}

class ProxyService {
  private operationTail: Promise<void> = Promise.resolve();
  private startupInitialization: Promise<void> | null = null;

  private validatePort(port: number): void {
    if (!Number.isInteger(port) || port < 0 || port > 65535) {
      throw new Error(`Port must be a valid u16 integer (0-65535), got: ${port}`);
    }
  }

  async startProxy(config: ProxyConfig): Promise<ProxyStatus> {
    try {
      this.validatePort(config.port);
      await ensureNativeTransportRoot(config.backend_url || DEFAULT_PROXY_BACKEND_URL);
      return await invoke<ProxyStatus>("start_proxy", { config });
    } catch (error) {
      console.error("Failed to start proxy:", error);
      throw error;
    }
  }

  initializeProxyOnStartup(): Promise<void> {
    if (this.startupInitialization) return this.startupInitialization;

    this.startupInitialization = (async () => {
      const config = await this.loadProxyConfig();
      if (config.auto_start && config.api_key.trim()) {
        await ensureNativeTransportRoot(config.backend_url || DEFAULT_PROXY_BACKEND_URL);
      }
      await invoke<void>("init_proxy_on_startup");
    })();
    return this.startupInitialization;
  }

  async stopProxy(): Promise<ProxyStatus> {
    try {
      return await invoke<ProxyStatus>("stop_proxy");
    } catch (error) {
      console.error("Failed to stop proxy:", error);
      throw error;
    }
  }

  async getProxyStatus(): Promise<ProxyStatus> {
    try {
      return await invoke<ProxyStatus>("get_proxy_status");
    } catch (error) {
      console.error("Failed to get proxy status:", error);
      throw error;
    }
  }

  async loadProxyConfig(): Promise<ProxyConfig> {
    try {
      return await invoke<ProxyConfig>("load_proxy_config");
    } catch (error) {
      console.error("Failed to load proxy config:", error);
      // Return default config if loading fails
      return {
        host: "127.0.0.1",
        port: 8080,
        api_key: "",
        enabled: false,
        enable_cors: false
      };
    }
  }

  async saveProxySettings(config: ProxyConfig): Promise<void> {
    try {
      this.validatePort(config.port);
      await invoke("save_proxy_settings", { config });
    } catch (error) {
      console.error("Failed to save proxy settings:", error);
      throw error;
    }
  }

  async testProxyPort(host: string, port: number): Promise<boolean> {
    try {
      this.validatePort(port);
      return await invoke<boolean>("test_proxy_port", { host, port });
    } catch (error) {
      console.error("Failed to test proxy port:", error);
      throw error;
    }
  }

  async startManualProxy(config: ProxyConfig): Promise<ProxyStatus> {
    return await this.enqueueProxyOperation(async () => {
      const status = await this.startProxy(config);
      if (!status.running || !manualProxyConfigsMatch(status.config, config)) {
        throw new Error(
          "The local proxy changed while the manual setup was starting. Review the current settings and try again."
        );
      }
      return status;
    });
  }

  async saveManualProxySettings(config: ProxyConfig): Promise<void> {
    await this.enqueueProxyOperation(async () => await this.saveProxySettings(config));
  }

  async stopManualProxy(): Promise<ProxyStatus> {
    return await this.enqueueProxyOperation(async () => await this.stopProxy());
  }

  // Stop the proxy and scrub its saved local credentials. This is local-only,
  // so an offline backend can never prevent logout.
  async stopAndResetProxy(): Promise<void> {
    if (!isTauriDesktop()) return;

    await this.enqueueProxyOperation(async () => {
      try {
        await invoke<ProxyStatus>("stop_and_reset_proxy");
      } catch (error) {
        console.error("Failed to stop and reset proxy:", error);
        throw error;
      }
    });
  }

  private async enqueueProxyOperation<T>(operation: () => Promise<T>): Promise<T> {
    const queued = this.operationTail.then(operation);
    this.operationTail = queued.then(
      () => undefined,
      () => undefined
    );
    return await queued;
  }
}

function normalizeBackendUrl(value?: string): string {
  return (value || "").trim().replace(/\/+$/, "");
}

export const proxyService = new ProxyService();
