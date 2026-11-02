import { createFileRoute, Navigate } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import { UsageSettings } from "@/components/settings/usage/UsageSettings";
import { FEATURE_FLAGS } from "@/services/flags";
import { useFeatureFlag } from "@/services/useFeatureFlag";

export const Route = createFileRoute("/settings/usage")({
  component: UsageRoute
});

// Gated by the usage_page flag while the page rolls out.
function UsageRoute() {
  const os = useOpenSecret();
  const enabled = useFeatureFlag(os.auth.user?.user.id, FEATURE_FLAGS.USAGE_PAGE);
  if (enabled === undefined) return null;
  if (!enabled) return <Navigate to="/settings/billing" replace />;
  return <UsageSettings />;
}
