import { Navigate, createFileRoute } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import { AppEntryPage } from "@/components/AppEntryPage";
import { AgentModeMovedPage } from "@/components/AgentModeMovedPage";
import { useRouteMeta } from "@/utils/routeMeta";
import { appUrl } from "@/config/domains";
import { isTauriDesktop } from "@/utils/platform";

export const Route = createFileRoute("/agent")({
  component: AgentRoute
});

function AgentRoute() {
  const os = useOpenSecret();

  useRouteMeta({
    title: "Maple",
    description: "Agent Mode is now Maple Work.",
    canonicalUrl: appUrl("/agent")
  });

  if (!isTauriDesktop()) {
    return <Navigate to="/" replace />;
  }

  if (!os.auth.user) {
    return <AppEntryPage />;
  }

  return <AgentModeMovedPage userId={os.auth.user.user.id} />;
}
