import { useCallback, useState, type MouseEvent } from "react";
import { Download, ExternalLink } from "lucide-react";

import { MapleWordmark } from "@/components/MapleWordmark";
import { ResizableSidebarLayout } from "@/components/ResizableSidebarLayout";
import { Sidebar, SidebarToggle } from "@/components/Sidebar";
import { Button } from "@/components/ui/button";
import { marketingUrl } from "@/config/domains";
import { usePersistentSidebarState } from "@/contexts/PersistentHomeNavigationContext";
import { GITHUB_REPOSITORY_URL } from "@/utils/githubRelease";
import { openExternalUrl } from "@/utils/openUrl";
import { useIsLandscapeMobile, useIsMobile } from "@/utils/utils";

const MAPLE_WORK_DOWNLOAD_URL = marketingUrl("/work/download");
const AGENT_MODE_RELEASE_URL = `${GITHUB_REPOSITORY_URL}/releases/tag/v3.4.5`;

function openInBrowser(event: MouseEvent<HTMLAnchorElement>) {
  event.preventDefault();
  void openExternalUrl(event.currentTarget.href);
}

/** Static Agent page content: sends Agent Mode beta testers to Maple Work. */
export function AgentModeMovedNotice() {
  return (
    <div className="mx-auto w-full max-w-xl space-y-6">
      <h1 className="pb-1 font-displayWide text-3xl font-normal leading-tight brand-gradient-text landscape-short:text-2xl">
        Agent Mode is now Maple Work
      </h1>
      <p className="text-base leading-relaxed text-muted-foreground">
        Thank you for beta testing Agent Mode. Your feedback went a long way: it shaped Maple Work,
        our private AI that works on your computer. Point it at a project and it reads your files,
        runs commands and finishes the task.
      </p>
      <Button asChild variant="primary" className="gap-2">
        <a href={MAPLE_WORK_DOWNLOAD_URL} onClick={openInBrowser}>
          <Download aria-hidden="true" className="h-4 w-4" />
          Download Maple Work
        </a>
      </Button>
      <div className="space-y-3 border-t border-border/60 pt-6 text-sm leading-relaxed">
        <p className="text-muted-foreground">
          Your Agent Mode tasks don’t move to Maple Work. They’re still on this computer. To open
          them, install Maple v3.4.5 from GitHub and sign in with this account. Skip its update
          prompt (or turn off Automatic updates in Settings → About) so it stays on v3.4.5.
        </p>
        <a
          href={AGENT_MODE_RELEASE_URL}
          onClick={openInBrowser}
          className="inline-flex items-center gap-1.5 font-medium text-[hsl(var(--maple-primary))] underline-offset-4 hover:underline"
        >
          Maple v3.4.5 on GitHub
          <ExternalLink aria-hidden="true" className="h-3.5 w-3.5" />
        </a>
      </div>
    </div>
  );
}

export function AgentModeMovedPage({ userId }: { userId: string }) {
  const isMobile = useIsMobile();
  const isLandscapeMobile = useIsLandscapeMobile();
  const isCompactLayout = isMobile || isLandscapeMobile;
  const [isSidebarOpen, setIsSidebarOpen] = usePersistentSidebarState(isCompactLayout);
  const [isSidebarTransitioning, setIsSidebarTransitioning] = useState(false);
  const toggleSidebar = useCallback(() => setIsSidebarOpen((prev) => !prev), [setIsSidebarOpen]);

  return (
    <ResizableSidebarLayout
      isCompactLayout={isCompactLayout}
      isOpen={isSidebarOpen}
      mode="agent"
      onOpenChange={setIsSidebarOpen}
      onTransitionChange={setIsSidebarTransitioning}
      sidebar={<Sidebar isOpen={isSidebarOpen} mode="agent" onToggle={toggleSidebar} />}
      userId={userId}
    >
      <div className="relative flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden bg-background">
        {!isSidebarOpen && !isSidebarTransitioning && (
          <div className="fixed left-4 top-[9.5px] z-20 flex items-center gap-1.5">
            <SidebarToggle onToggle={toggleSidebar} />
            <MapleWordmark
              className="h-4 w-auto animate-in fade-in-0 slide-in-from-left-1 duration-300"
              aria-hidden
            />
          </div>
        )}
        <main className="flex min-h-0 flex-1 flex-col overflow-y-auto px-6 py-16 landscape-short:py-12">
          <div className="my-auto">
            <AgentModeMovedNotice />
          </div>
        </main>
      </div>
    </ResizableSidebarLayout>
  );
}
