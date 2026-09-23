import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import { AlertTriangle, Users } from "lucide-react";
import { Button } from "@/components/ui/button";
import { getBillingService } from "@/billing/billingService";
import type { TeamStatus } from "@/types/team";
import { isIOS } from "@/utils/platform";
import { formatTeamSeatMismatchMessage, getTeamSeatMismatch } from "@/utils/teamSeats";

export function TeamSeatMismatchAlert() {
  const os = useOpenSecret();
  const navigate = useNavigate();

  const { data: teamStatus } = useQuery<TeamStatus>({
    queryKey: ["teamStatus"],
    queryFn: async () => {
      const billingService = getBillingService();
      return await billingService.getTeamStatus();
    },
    enabled: !!os.auth.user,
    refetchOnWindowFocus: true
  });

  const mismatch = getTeamSeatMismatch(teamStatus);
  if (!mismatch) return null;

  const isAdmin = teamStatus?.role === "admin" || teamStatus?.is_team_admin === true;
  const summary =
    mismatch.hasExactCounts &&
    mismatch.memberCount !== null &&
    mismatch.billedSeatCount !== null &&
    mismatch.memberCount > mismatch.billedSeatCount
      ? `${mismatch.memberCount} ${
          mismatch.memberCount === 1 ? "member" : "members"
        }, ${mismatch.billedSeatCount} paid ${mismatch.billedSeatCount === 1 ? "seat" : "seats"}`
      : "Seat count mismatch";

  return (
    <div className="pointer-events-none fixed left-3 right-3 top-20 z-40 sm:left-auto sm:right-4 sm:w-[24rem]">
      <div className="pointer-events-auto rounded-lg border border-destructive/50 bg-card p-4 text-card-foreground shadow-lg">
        <div className="flex items-start gap-3">
          <AlertTriangle className="mt-0.5 h-5 w-5 flex-shrink-0 text-destructive" />
          <div className="min-w-0 flex-1 space-y-3">
            <div>
              <h4 className="text-sm font-medium leading-tight">Team usage paused</h4>
              <p className="mt-1 text-xs font-medium text-destructive">{summary}</p>
              <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
                {formatTeamSeatMismatchMessage(mismatch, isAdmin ? "admin" : "member", !isIOS())}
              </p>
            </div>
            <div className="flex justify-end">
              <Button
                size="sm"
                onClick={() => {
                  navigate({ to: "/settings/team" });
                }}
              >
                <Users className="mr-1.5 h-3.5 w-3.5" />
                {isAdmin ? "Manage Team" : "Team Info"}
              </Button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
