import { useEffect, useMemo, useRef, useState } from "react";
import packageJson from "../../../package.json";
import { buttonVariants } from "@/components/ui/button";
import {
  APP_DOWNLOAD_TARGETS,
  appDownloadActions,
  appDownloadCopy,
  appDownloadTargetLabel,
  detectAppDownloadTarget,
  readAppDownloadEnvironment,
  type AppDownloadEnvironment,
  type AppDownloadTarget
} from "@/utils/appDownloads";
import {
  buildFallbackDownloadInfo,
  getLatestDownloadInfo,
  type DownloadInfo
} from "@/utils/githubRelease";
import { cn } from "@/utils/utils";
import { SettingsSection } from "./SettingsPage";

const DEFAULT_TARGET: AppDownloadTarget = "macos";

function nextDownloadTarget(current: AppDownloadTarget, key: string): AppDownloadTarget | null {
  const index = APP_DOWNLOAD_TARGETS.indexOf(current);
  if (key === "Home") {
    return APP_DOWNLOAD_TARGETS[0] ?? DEFAULT_TARGET;
  }
  if (key === "End") {
    return APP_DOWNLOAD_TARGETS[APP_DOWNLOAD_TARGETS.length - 1] ?? DEFAULT_TARGET;
  }
  if (key === "ArrowRight" || key === "ArrowDown") {
    return APP_DOWNLOAD_TARGETS[(index + 1) % APP_DOWNLOAD_TARGETS.length] ?? DEFAULT_TARGET;
  }
  if (key === "ArrowLeft" || key === "ArrowUp") {
    return (
      APP_DOWNLOAD_TARGETS[
        (index - 1 + APP_DOWNLOAD_TARGETS.length) % APP_DOWNLOAD_TARGETS.length
      ] ?? DEFAULT_TARGET
    );
  }
  return null;
}

export type DownloadSettingsProps = {
  environment?: AppDownloadEnvironment;
  loadDownloadInfo?: (signal?: AbortSignal) => Promise<DownloadInfo | null>;
};

export function DownloadSettings({
  environment,
  loadDownloadInfo = getLatestDownloadInfo
}: DownloadSettingsProps) {
  const resolvedEnvironment = environment ?? readAppDownloadEnvironment();
  const detectedTarget = detectAppDownloadTarget(resolvedEnvironment);
  const fallbackInfo = useMemo(() => buildFallbackDownloadInfo(packageJson.version), []);
  const [selectedTarget, setSelectedTarget] = useState<AppDownloadTarget>(
    detectedTarget ?? DEFAULT_TARGET
  );
  const [downloadInfo, setDownloadInfo] = useState<DownloadInfo>(fallbackInfo);
  const [isLoading, setIsLoading] = useState(true);
  const targetButtonRefs = useRef<Partial<Record<AppDownloadTarget, HTMLButtonElement | null>>>({});

  const selectTarget = (target: AppDownloadTarget) => {
    setSelectedTarget(target);
    targetButtonRefs.current[target]?.focus();
  };

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();

    const load = async () => {
      try {
        const latest = await loadDownloadInfo(controller.signal);
        if (!cancelled && latest) {
          setDownloadInfo(latest);
        }
      } catch (error) {
        if (!cancelled) {
          console.error("Failed to load latest Maple download info:", error);
        }
      } finally {
        if (!cancelled) {
          setIsLoading(false);
        }
      }
    };

    void load();

    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [loadDownloadInfo]);

  const copy = appDownloadCopy(selectedTarget);
  const actions = appDownloadActions(selectedTarget, downloadInfo.downloadUrls);

  return (
    <SettingsSection
      title="Get the Maple app"
      description="Install Maple on your computer or phone. Native apps include document upload; the desktop app also has automatic updates."
    >
      <div className="space-y-5">
        <p className="text-xs leading-relaxed text-muted-foreground">
          {detectedTarget
            ? `This browser looks like ${appDownloadTargetLabel(detectedTarget)}. Choose another platform if you want a different installer.`
            : "Choose the platform you want to install."}
        </p>

        <div role="radiogroup" aria-label="Download platform" className="flex flex-wrap gap-2">
          {APP_DOWNLOAD_TARGETS.map((target) => {
            const selected = target === selectedTarget;
            return (
              <button
                key={target}
                type="button"
                role="radio"
                aria-checked={selected}
                tabIndex={selected ? 0 : -1}
                ref={(node) => {
                  targetButtonRefs.current[target] = node;
                }}
                onClick={() => selectTarget(target)}
                onKeyDown={(event) => {
                  const next = nextDownloadTarget(target, event.key);
                  if (!next) {
                    return;
                  }
                  event.preventDefault();
                  selectTarget(next);
                }}
                className={cn(
                  "rounded-lg border px-3 py-1.5 text-sm font-medium transition-colors",
                  selected
                    ? "border-[hsl(var(--maple-primary))] bg-[hsl(var(--maple-primary-container))]/70 text-foreground"
                    : "border-border/70 text-muted-foreground hover:border-[hsl(var(--maple-primary))]/60 hover:bg-muted/50"
                )}
              >
                {appDownloadTargetLabel(target)}
                {target === detectedTarget ? (
                  <span className="ml-2 text-[10px] font-semibold uppercase tracking-wide text-[hsl(var(--maple-primary))]">
                    Suggested
                  </span>
                ) : null}
              </button>
            );
          })}
        </div>

        <div className="flex flex-col gap-3 border-t border-border/70 pt-4 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <p className="text-sm font-medium">{copy.title}</p>
            <p className="mt-1 text-xs leading-relaxed text-muted-foreground">{copy.description}</p>
          </div>
          <div className="flex flex-wrap gap-2 sm:justify-end">
            {actions.map((action) => (
              <a
                key={`${selectedTarget}-${action.label}`}
                className={buttonVariants({
                  variant: action.variant === "primary" ? "primary" : "outline",
                  size: "sm"
                })}
                href={action.href}
                target="_blank"
                rel="noopener noreferrer"
              >
                {action.label}
              </a>
            ))}
          </div>
        </div>

        <p className="text-xs leading-relaxed text-muted-foreground">
          Current version: <span className="font-mono text-foreground">{downloadInfo.version}</span>
          {isLoading ? " (loading...)" : null} •{" "}
          <a
            href={downloadInfo.releaseUrl}
            className="text-[hsl(var(--maple-primary))] underline-offset-4 hover:underline"
            target="_blank"
            rel="noopener noreferrer"
          >
            Release notes
          </a>
        </p>
      </div>
    </SettingsSection>
  );
}
