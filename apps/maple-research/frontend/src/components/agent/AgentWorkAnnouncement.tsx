import { useEffect, useState } from "react";
import { arch, type } from "@tauri-apps/plugin-os";
import { ArrowRight, Download, ExternalLink, Gift, Sparkles } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger
} from "@/components/ui/dialog";
import { marketingUrl } from "@/config/domains";
import { FEATURE_FLAGS, flagsClient } from "@/services/flags";
import { openExternalUrl } from "@/utils/openUrl";

function workDownload() {
  // These are the architectures currently published by Maple Work. Read the
  // native OS plugin rather than guessing from the webview's user agent.
  try {
    const os = type();
    const architecture = arch();
    if (os === "macos" && architecture === "aarch64") {
      return {
        label: "Download for macOS",
        detail: "For Apple silicon Macs",
        url: "https://updates.maple.ai/download/agent/stable/macos"
      };
    }
    if (os === "linux" && architecture === "x86_64") {
      return {
        label: "Download for Linux",
        detail: "For x86-64 Linux · Extract the archive and run install.sh",
        url: "https://updates.maple.ai/download/agent/stable/linux"
      };
    }
  } catch {
    // Unknown devices can still learn about Work without a mismatched installer.
  }
  return null;
}

export function AgentWorkAnnouncement({ userId }: { userId: string }) {
  const [flag, setFlag] = useState<{ userId: string; enabled: boolean } | null>(null);
  const enabled =
    flag?.userId === userId
      ? flag.enabled
      : flagsClient.peekIsEnabled(userId, FEATURE_FLAGS.AGENT_MODE);

  useEffect(() => {
    let disposed = false;
    void flagsClient.isEnabled(userId, FEATURE_FLAGS.AGENT_MODE).then(
      (enabled) => {
        if (!disposed) setFlag({ userId, enabled });
      },
      () => {
        if (!disposed) setFlag({ userId, enabled: false });
      }
    );
    return () => {
      disposed = true;
    };
  }, [userId]);

  if (enabled !== true) return null;

  const download = workDownload();

  return (
    <div className="mx-auto mb-4 mt-14 flex w-full max-w-3xl shrink-0 justify-center px-4">
      <Dialog>
        <DialogTrigger asChild>
          <button
            type="button"
            className="group flex max-w-full flex-wrap items-center justify-center gap-x-3 gap-y-1 rounded-full border border-[hsl(var(--maple-primary))]/30 bg-[hsl(var(--maple-primary))]/10 px-5 py-3 text-sm transition-colors hover:bg-[hsl(var(--maple-primary))]/15 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2"
          >
            <Sparkles
              aria-hidden="true"
              className="h-4 w-4 shrink-0 text-[hsl(var(--maple-primary))]"
            />
            <span className="font-medium">Agent Mode is moving to a new app</span>
            <span className="inline-flex items-center gap-1 font-semibold text-foreground">
              Learn more
              <ArrowRight aria-hidden="true" className="h-3.5 w-3.5" />
            </span>
          </button>
        </DialogTrigger>

        <DialogContent className="max-h-[85dvh] overflow-y-auto sm:max-w-xl">
          <DialogHeader>
            <div className="mb-2 flex items-center gap-3 pr-6">
              <div className="rounded-lg bg-primary/10 p-2 text-primary">
                <Sparkles aria-hidden="true" className="h-7 w-7" />
              </div>
              <DialogTitle className="text-2xl">Meet Maple Work</DialogTitle>
            </div>
            <DialogDescription className="text-base">
              Agent Mode is moving to its own desktop app.
            </DialogDescription>
          </DialogHeader>

          <div className="space-y-4 py-1 text-sm leading-relaxed">
            <div className="space-y-2">
              <p className="font-medium">Thanks for being an early beta tester!</p>
              <p className="text-muted-foreground">
                Maple Work brings private AI to your desktop to work in your projects, run commands,
                and get tasks done for you.
              </p>
            </div>

            <div className="space-y-2 rounded-xl border border-[hsl(var(--maple-primary))]/25 bg-[hsl(var(--maple-primary))]/10 p-4">
              <p className="flex items-center gap-2 font-semibold">
                <Gift aria-hidden="true" className="h-4 w-4 shrink-0" />
                50% off DeepSeek for a limited time
              </p>
              <p className="text-muted-foreground">
                For a limited time, enjoy special pricing on DeepSeek usage in Maple Work. Your
                early-tester pricing is already active on your account.
              </p>
            </div>

            <div className="space-y-2 border-t pt-4">
              <p className="font-semibold">What happens to your Agent Mode chats?</p>
              <p className="text-muted-foreground">
                Agent Mode in Maple Research will be retired soon. You won’t be able to continue
                your chats here after that, and they won’t transfer to Maple Work. Save anything you
                want to keep.
              </p>
            </div>
          </div>

          <DialogFooter className="flex-col gap-2 sm:flex-col sm:space-x-0">
            {download ? (
              <>
                <Button
                  variant="primary"
                  className="w-full gap-2"
                  onClick={() => void openExternalUrl(download.url)}
                >
                  <Download aria-hidden="true" className="h-4 w-4" />
                  {download.label}
                </Button>
                <p className="text-center text-xs text-muted-foreground">{download.detail}</p>
              </>
            ) : (
              <p className="text-center text-xs text-muted-foreground">
                Maple Work is available for Apple silicon Macs and x86-64 Linux. A download for this
                device isn’t available yet.
              </p>
            )}
            <Button
              variant="ghost"
              className="w-full gap-2"
              onClick={() => void openExternalUrl(marketingUrl("/work"))}
            >
              Learn more about Maple Work
              <ExternalLink aria-hidden="true" className="h-3.5 w-3.5" />
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
