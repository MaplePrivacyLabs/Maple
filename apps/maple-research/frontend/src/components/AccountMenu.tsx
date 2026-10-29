import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import { AlertCircle, Settings } from "lucide-react";
import { getBillingService } from "@/billing/billingService";
import { CreditUsage } from "@/components/CreditUsage";
import { useCompactSettingsLayout } from "@/components/settings/useCompactSettingsLayout";
import { useBillingState } from "@/state/useLocalState";
import type { TeamStatus } from "@/types/team";
import { getTeamSeatMismatch } from "@/utils/teamSeats";

export function AccountMenu() {
  const os = useOpenSecret();
  const { billingStatus, setBillingStatus } = useBillingState();
  const isCompactSettingsLayout = useCompactSettingsLayout();
  const isTeamPlan = billingStatus?.product_name?.toLowerCase().includes("team") ?? false;

  // Keep the shared sidebar billing badge current on every authenticated route.
  // Some routes do not own a route-level billing refresh.
  useQuery({
    queryKey: ["billingStatus"],
    queryFn: async () => {
      const status = await getBillingService().getBillingStatus();
      setBillingStatus(status);
      return status;
    },
    enabled: !!os.auth.user
  });

  const { data: teamStatus } = useQuery<TeamStatus>({
    queryKey: ["teamStatus"],
    queryFn: () => getBillingService().getTeamStatus(),
    enabled: isTeamPlan && !!os.auth.user && !!billingStatus
  });

  const needsTeamSetup = !!teamStatus?.has_team_subscription && teamStatus.team_created === false;
  const teamSeatMismatch = getTeamSeatMismatch(teamStatus);
  const attentionLabel = teamSeatMismatch
    ? "Team usage paused"
    : needsTeamSetup
      ? "Team setup required"
      : undefined;

  return (
    <div className="flex w-full max-w-full items-end gap-2">
      <Link
        to={isCompactSettingsLayout ? "/settings" : "/settings/account"}
        aria-label={attentionLabel ? `Open settings, ${attentionLabel}` : "Open settings"}
        title="Settings"
        className="relative flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-[hsl(var(--sidebar-chrome))] text-[hsl(var(--on-sidebar-chrome))] shadow-none ring-0 transition-colors hover:bg-[hsl(var(--sidebar-chrome-hover))] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        <Settings className="h-4 w-4" />
        {(teamSeatMismatch || needsTeamSetup) && (
          <AlertCircle
            className={`absolute -right-1 -top-1 h-4 w-4 rounded-full bg-background ${
              teamSeatMismatch ? "text-destructive" : "text-maple-warning"
            }`}
          />
        )}
      </Link>
      <Link
        to="/pricing"
        className="group/credit-link min-w-0 flex-1 rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background"
        aria-label={billingStatus ? `${billingStatus.product_name} plan` : "Billing status"}
      >
        <CreditUsage />
      </Link>
    </div>
  );
}
