import { useEffect, useRef, useState } from "react";
import { RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
  updateService,
  type PreparedUpdate,
  type UpdateCheckResult,
  type UpdaterPreferences,
  type UpdateService
} from "@/services/updateService";
import { cn } from "@/utils/utils";
import { SettingsSection } from "./SettingsPage";

function updateCheckMessage(result: UpdateCheckResult): string {
  switch (result.status) {
    case "up_to_date":
      return "Maple is up to date.";
    case "ready_to_restart":
      return `Version ${result.version} is installed. Restart Maple to finish updating.`;
    case "ready_to_install":
      return `Version ${result.version} is downloaded and ready to install.`;
    case "updates_disabled_for_profile":
      return "Updates are disabled for this Maple Dev build. Install a new Dev build to update.";
    case "automatic_updates_disabled":
      return "Automatic updates are off. You can still check for updates manually.";
  }
}

export function UpdateSettings({ service = updateService }: { service?: UpdateService }) {
  const mountedRef = useRef(false);
  const [preferences, setPreferences] = useState<UpdaterPreferences | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [isSaving, setIsSaving] = useState(false);
  const [isChecking, setIsChecking] = useState(false);
  const [preferenceError, setPreferenceError] = useState<string | null>(null);
  const [preferenceStatus, setPreferenceStatus] = useState<string | null>(null);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [checkStatus, setCheckStatus] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [loadFailed, setLoadFailed] = useState(false);
  const [loadAttempt, setLoadAttempt] = useState(0);
  const [preparedUpdate, setPreparedUpdate] = useState<PreparedUpdate | null>(null);
  const [preparedUpdateError, setPreparedUpdateError] = useState<string | null>(null);
  const [updateOperation, setUpdateOperation] = useState<"installing" | "restarting" | null>(null);
  const preparedUpdateGenerationRef = useRef(0);

  useEffect(() => {
    mountedRef.current = true;

    const loadPreferences = async () => {
      try {
        const loaded = await service.loadPreferences();
        if (mountedRef.current) {
          setPreferences(loaded);
          setLoadFailed(false);
          setLoadError(null);
        }
      } catch (loadError) {
        console.error("Failed to load updater preferences:", loadError);
        if (mountedRef.current) {
          setPreferences(null);
          setLoadFailed(true);
          setLoadError(
            "Maple couldn't confirm your update preference. Automatic update behavior is unchanged. Try again, or turn automatic updates off now."
          );
        }
      } finally {
        if (mountedRef.current) setIsLoading(false);
      }
    };

    void loadPreferences();
    return () => {
      mountedRef.current = false;
    };
  }, [service, loadAttempt]);

  useEffect(() => {
    let disposed = false;
    let unsubscribe: (() => void) | null = null;

    const receivePreparedUpdate = (prepared: PreparedUpdate) => {
      if (!disposed) {
        preparedUpdateGenerationRef.current += 1;
        setPreparedUpdate(prepared);
        setPreparedUpdateError(null);
        setCheckStatus(null);
        setCheckError(null);
      }
    };

    const loadPreparedUpdate = async () => {
      try {
        const subscribed = await service.subscribePreparedUpdates(receivePreparedUpdate);
        if (disposed) {
          subscribed();
        } else {
          unsubscribe = subscribed;
        }
      } catch (subscriptionError) {
        console.error("Failed to subscribe to prepared updater state:", subscriptionError);
      }

      const queryGeneration = preparedUpdateGenerationRef.current;
      try {
        const prepared = await service.getPreparedUpdate();
        if (!disposed && preparedUpdateGenerationRef.current === queryGeneration) {
          setPreparedUpdate(prepared);
        }
      } catch (preparedError) {
        console.error("Failed to load prepared updater state:", preparedError);
        if (!disposed && preparedUpdateGenerationRef.current === queryGeneration) {
          setPreparedUpdateError(
            "Maple couldn't confirm whether an update is ready. Check again to refresh its state."
          );
        }
      }
    };

    void loadPreparedUpdate();

    return () => {
      disposed = true;
      unsubscribe?.();
    };
  }, [service]);

  const setAutomaticUpdates = async (automaticUpdates: boolean) => {
    if (isSaving) return;

    const next = { automatic_updates: automaticUpdates };
    setIsSaving(true);
    setPreferenceError(null);
    setPreferenceStatus(null);

    try {
      await service.savePreferences(next);
      if (mountedRef.current) {
        setPreferences(next);
        setLoadFailed(false);
        setLoadError(null);
        setPreferenceStatus(
          automaticUpdates
            ? "Automatic updates are on. Maple will check shortly after launch and every hour."
            : "Automatic updates are off. You can still check for updates manually."
        );
      }
    } catch (saveError) {
      console.error("Failed to save updater preferences:", saveError);
      if (mountedRef.current) {
        setPreferenceError("Maple couldn't save your update preference. Please try again.");
      }
    } finally {
      if (mountedRef.current) setIsSaving(false);
    }
  };

  const retryLoadingPreferences = () => {
    setIsLoading(true);
    setLoadError(null);
    setPreferenceStatus(null);
    setLoadAttempt((attempt) => attempt + 1);
  };

  const checkForUpdates = async () => {
    if (isChecking) return;

    setIsChecking(true);
    setCheckError(null);
    setCheckStatus(null);
    setPreparedUpdateError(null);

    try {
      const result = await service.checkForUpdates();
      if (mountedRef.current) {
        if (result.status === "ready_to_install" || result.status === "ready_to_restart") {
          setPreparedUpdate(result);
        } else {
          setCheckStatus(updateCheckMessage(result));
        }
      }
    } catch (checkError) {
      console.error("Failed to check for updates:", checkError);
      if (mountedRef.current) {
        setCheckError(
          "Maple couldn't complete the update check. Try again; if it keeps failing, use the latest installer."
        );
      }
    } finally {
      if (mountedRef.current) setIsChecking(false);
    }
  };

  const installPreparedUpdate = async () => {
    if (updateOperation || preparedUpdate?.status !== "ready_to_install") return;

    const expectedVersion = preparedUpdate.version;
    setUpdateOperation("installing");
    setPreparedUpdateError(null);

    try {
      const prepared = await service.installPreparedUpdate(expectedVersion);
      if (mountedRef.current) {
        setPreparedUpdate(prepared);
        if (prepared.status === "ready_to_install") {
          setPreparedUpdateError(
            `Maple couldn't install version ${expectedVersion}. The verified download is still ready; try again.`
          );
        }
      }
    } catch (installError) {
      console.error("Failed to install prepared update:", installError);
      const reconciliationGeneration = preparedUpdateGenerationRef.current;
      try {
        const currentPrepared = await service.getPreparedUpdate();
        if (
          mountedRef.current &&
          preparedUpdateGenerationRef.current === reconciliationGeneration
        ) {
          setPreparedUpdate(currentPrepared);
          if (currentPrepared?.status === "ready_to_restart") {
            // Another visible surface may have completed this same install
            // while this command waited on the native single-flight lock.
            setPreparedUpdateError(null);
          } else if (
            currentPrepared?.status === "ready_to_install" &&
            currentPrepared.version === expectedVersion
          ) {
            setPreparedUpdateError(
              `Maple couldn't install version ${expectedVersion}. The verified download is still ready; try again.`
            );
          } else {
            setPreparedUpdateError(
              "Maple couldn't confirm the downloaded update after installation failed. Check again to refresh its state."
            );
          }
        }
      } catch (preparedError) {
        console.error("Failed to reconcile prepared updater state:", preparedError);
        if (
          mountedRef.current &&
          preparedUpdateGenerationRef.current === reconciliationGeneration
        ) {
          setPreparedUpdateError(
            "Maple couldn't confirm the downloaded update after installation failed. Check again to refresh its state."
          );
        }
      }
    } finally {
      if (mountedRef.current) setUpdateOperation(null);
    }
  };

  const restartForUpdate = async () => {
    if (updateOperation || preparedUpdate?.status !== "ready_to_restart") return;

    setUpdateOperation("restarting");
    setPreparedUpdateError(null);

    try {
      await service.restartForUpdate();
    } catch (restartError) {
      console.error("Failed to restart for update:", restartError);
      if (mountedRef.current) {
        setPreparedUpdateError(
          "Maple couldn't restart automatically. Quit and reopen Maple to finish updating."
        );
      }
    } finally {
      if (mountedRef.current) setUpdateOperation(null);
    }
  };

  return (
    <SettingsSection
      title="Updates"
      description="Choose whether Maple checks for updates in the background, or check whenever you want."
    >
      <div className="space-y-5">
        <div className="flex items-start justify-between gap-4">
          <div>
            <Label htmlFor="automatic-updates">Automatic updates</Label>
            <p
              id="automatic-updates-description"
              className="mt-1 text-xs leading-relaxed text-muted-foreground"
            >
              With this on, Maple checks shortly after launch and every hour, then downloads
              available updates. You choose when to install them. Linux deb/rpm packages keep their
              system install prompt.
            </p>
          </div>
          <Switch
            id="automatic-updates"
            checked={preferences?.automatic_updates ?? false}
            onCheckedChange={(checked) => void setAutomaticUpdates(checked)}
            disabled={isLoading || isSaving || !preferences}
            aria-describedby="automatic-updates-description"
            aria-busy={isSaving}
          />
        </div>

        <div className="flex flex-col gap-3 border-t border-border/70 pt-4 sm:flex-row sm:items-center sm:justify-between">
          <p className="text-xs leading-relaxed text-muted-foreground">
            A manual check downloads an available update even when automatic updates are off. Maple
            waits for you to install it; Linux deb/rpm packages ask for system approval at that
            point.
          </p>
          <Button
            type="button"
            variant="outline"
            onClick={() => void checkForUpdates()}
            disabled={isChecking || updateOperation !== null}
            aria-busy={isChecking}
          >
            <RefreshCw
              aria-hidden="true"
              className={cn(
                "mr-2 h-4 w-4",
                isChecking && "animate-spin motion-reduce:animate-none"
              )}
            />
            {isChecking ? "Checking..." : "Check for updates"}
          </Button>
        </div>

        {preparedUpdate && (
          <div
            className="flex flex-col gap-3 rounded-lg border border-border bg-muted/30 p-4 sm:flex-row sm:items-center sm:justify-between"
            role="status"
            aria-live="polite"
          >
            <div>
              <p className="text-sm font-medium">
                {preparedUpdate.status === "ready_to_install"
                  ? "Update ready to install"
                  : "Restart to finish updating"}
              </p>
              <p
                id="prepared-update-description"
                className="mt-1 text-xs leading-relaxed text-muted-foreground"
              >
                {preparedUpdate.status === "ready_to_install" ? (
                  <>
                    Version {preparedUpdate.version} is downloaded and signature-verified.
                    {preparedUpdate.requires_system_approval &&
                      " Your system will ask for approval to install it."}
                  </>
                ) : (
                  <>Version {preparedUpdate.version} is installed. Restart Maple to apply it.</>
                )}
              </p>
            </div>
            {preparedUpdate.status === "ready_to_install" ? (
              <Button
                type="button"
                size="sm"
                onClick={() => void installPreparedUpdate()}
                disabled={updateOperation !== null}
                aria-busy={updateOperation === "installing"}
                aria-describedby="prepared-update-description"
              >
                {updateOperation === "installing" ? "Installing..." : "Install now"}
              </Button>
            ) : (
              <Button
                type="button"
                size="sm"
                onClick={() => void restartForUpdate()}
                disabled={updateOperation !== null}
                aria-busy={updateOperation === "restarting"}
                aria-describedby="prepared-update-description"
              >
                {updateOperation === "restarting" ? "Restarting..." : "Restart Maple"}
              </Button>
            )}
          </div>
        )}

        {isLoading && (
          <p className="text-xs text-muted-foreground" role="status">
            Loading update preference...
          </p>
        )}
        {preferenceStatus && (
          <p className="text-xs leading-relaxed text-muted-foreground" role="status">
            {preferenceStatus}
          </p>
        )}
        {checkStatus && (
          <p className="text-xs leading-relaxed text-muted-foreground" role="status">
            {checkStatus}
          </p>
        )}
        {loadError && (
          <div className="space-y-3" role="alert">
            <p className="text-xs leading-relaxed text-destructive">{loadError}</p>
            {loadFailed && (
              <div className="flex flex-wrap gap-2">
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={retryLoadingPreferences}
                  disabled={isLoading || isSaving}
                >
                  Retry preference
                </Button>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => void setAutomaticUpdates(false)}
                  disabled={isSaving}
                >
                  Turn automatic updates off
                </Button>
              </div>
            )}
          </div>
        )}
        {preferenceError && (
          <p className="text-xs leading-relaxed text-destructive" role="alert">
            {preferenceError}
          </p>
        )}
        {checkError && (
          <p className="text-xs leading-relaxed text-destructive" role="alert">
            {checkError}
          </p>
        )}
        {preparedUpdateError && (
          <p className="text-xs leading-relaxed text-destructive" role="alert">
            {preparedUpdateError}
          </p>
        )}
      </div>
    </SettingsSection>
  );
}
