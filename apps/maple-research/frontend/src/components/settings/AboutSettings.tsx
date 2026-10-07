import { Link } from "@tanstack/react-router";
import { ExternalLink, FileText, Info, Mail, Shield } from "lucide-react";
import packageJson from "../../../package.json";
import { Button } from "@/components/ui/button";
import { openExternalUrl } from "@/utils/openUrl";
import { isAndroid, isTauriDesktop, isWeb } from "@/utils/platform";
import { SettingsPage, SettingsSection } from "./SettingsPage";
import { DownloadSettings } from "./DownloadSettings";
import { UpdateSettings } from "./UpdateSettings";

type ExternalRowProps = {
  label: string;
  url: string;
  icon: typeof Shield;
};

function ExternalRow({ label, url, icon: Icon }: ExternalRowProps) {
  return (
    <button
      type="button"
      onClick={() => void openExternalUrl(url)}
      className="flex w-full items-center gap-3 rounded-lg border border-border/70 p-3 text-left transition-colors hover:border-[hsl(var(--maple-primary))]/60 hover:bg-muted/50 sm:p-4"
    >
      <Icon className="h-4 w-4 shrink-0 text-muted-foreground" />
      <span className="min-w-0 flex-1 text-sm font-medium">{label}</span>
      <ExternalLink className="h-4 w-4 shrink-0 text-muted-foreground" />
    </button>
  );
}

export function resolveAboutSettingsGating(
  checks: { isTauriDesktop: () => boolean; isWeb: () => boolean } = {
    isTauriDesktop,
    isWeb
  }
) {
  return {
    supportsDesktopUpdates: checks.isTauriDesktop(),
    showAppDownloads: checks.isWeb()
  };
}

export function AboutSettings({
  supportsDesktopUpdates,
  showAppDownloads
}: {
  supportsDesktopUpdates?: boolean;
  showAppDownloads?: boolean;
} = {}) {
  const gating = resolveAboutSettingsGating();
  const showUpdates = supportsDesktopUpdates ?? gating.supportsDesktopUpdates;
  const showDownloads = showAppDownloads ?? gating.showAppDownloads;
  return (
    <SettingsPage title="About" description="Maple information, updates, policies, and support.">
      <SettingsSection title="Maple Research">
        <div className="flex flex-col gap-4 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <p className="text-sm leading-relaxed text-muted-foreground">
              A private AI workspace built for secure research and collaboration.
            </p>
            <p className="mt-2 text-xs text-muted-foreground">Version {packageJson.version}</p>
          </div>
          <Button asChild variant="outline">
            <Link to="/about">
              <Info className="mr-2 h-4 w-4" />
              About Maple
            </Link>
          </Button>
        </div>
      </SettingsSection>

      {showUpdates && <UpdateSettings />}
      {showDownloads && <DownloadSettings />}

      <SettingsSection title="Policies and support">
        <div className="space-y-2">
          {isAndroid() ? (
            <>
              <Button asChild variant="outline" className="w-full justify-start">
                <Link to="/privacy">
                  <Shield className="mr-3 h-4 w-4" />
                  Privacy policy
                </Link>
              </Button>
              <Button asChild variant="outline" className="w-full justify-start">
                <Link to="/terms">
                  <FileText className="mr-3 h-4 w-4" />
                  Terms of service
                </Link>
              </Button>
            </>
          ) : (
            <>
              <ExternalRow label="Privacy policy" url="https://trymaple.ai/privacy" icon={Shield} />
              <ExternalRow
                label="Terms of service"
                url="https://trymaple.ai/terms"
                icon={FileText}
              />
            </>
          )}
          <ExternalRow label="Contact us" url="mailto:support@trymaple.ai" icon={Mail} />
        </div>
      </SettingsSection>
    </SettingsPage>
  );
}
