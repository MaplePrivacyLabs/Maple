import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen } from "@tauri-apps/api/event";

export interface UpdaterPreferences {
  automatic_updates: boolean;
}

export type UpdateCheckResult =
  | { status: "automatic_updates_disabled" }
  | { status: "updates_disabled_for_profile" }
  | { status: "up_to_date" }
  | { status: "ready_to_restart"; version: string }
  | {
      status: "ready_to_install";
      version: string;
      requires_system_approval: boolean;
    };

export type PreparedUpdate =
  | {
      status: "ready_to_install";
      version: string;
      requires_system_approval: boolean;
    }
  | { status: "ready_to_restart"; version: string };

type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;
type Unlisten = () => void;
type Listen = <T>(event: string, handler: (event: { payload: T }) => void) => Promise<Unlisten>;

export interface UpdateService {
  loadPreferences(): Promise<UpdaterPreferences>;
  savePreferences(preferences: UpdaterPreferences): Promise<void>;
  checkForUpdates(): Promise<UpdateCheckResult>;
  getPreparedUpdate(): Promise<PreparedUpdate | null>;
  installPreparedUpdate(expectedVersion: string): Promise<PreparedUpdate>;
  restartForUpdate(): Promise<void>;
  subscribePreparedUpdates(handler: (update: PreparedUpdate) => void): Promise<Unlisten>;
}

export function createUpdateService(
  invoke: Invoke = tauriInvoke,
  listen: Listen = tauriListen as Listen
): UpdateService {
  return {
    loadPreferences: async () => (await invoke("load_updater_preferences")) as UpdaterPreferences,
    savePreferences: async (preferences) => {
      await invoke("save_updater_preferences", { preferences });
    },
    checkForUpdates: async () => (await invoke("check_for_updates_manually")) as UpdateCheckResult,
    getPreparedUpdate: async () => (await invoke("get_prepared_update")) as PreparedUpdate | null,
    installPreparedUpdate: async (expectedVersion) =>
      (await invoke("install_pending_update", {
        expectedVersion
      })) as PreparedUpdate,
    restartForUpdate: async () => {
      await invoke("restart_for_update");
    },
    subscribePreparedUpdates: async (handler) => {
      const unlistenAvailable = await listen<{
        version: string;
        requires_system_approval: boolean;
      }>("update-available", ({ payload }) => {
        handler({ ...payload, status: "ready_to_install" });
      });

      try {
        const unlistenReady = await listen<{ version: string }>("update-ready", ({ payload }) => {
          handler({ ...payload, status: "ready_to_restart" });
        });

        return () => {
          unlistenAvailable();
          unlistenReady();
        };
      } catch (error) {
        unlistenAvailable();
        throw error;
      }
    }
  };
}

export const updateService = createUpdateService();
