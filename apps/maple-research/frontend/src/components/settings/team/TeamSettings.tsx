import { useRef, useState } from "react";
import { Link, useBlocker } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useOpenSecret } from "@mapleai/sdk";
import {
  AlertCircle,
  AlertTriangle,
  Check,
  CreditCard,
  Crown,
  Loader2,
  LogOut,
  Pencil,
  RotateCw,
  User,
  UserPlus,
  Users,
  X
} from "lucide-react";
import { openBillingPortal } from "@/billing/billingPortal";
import { getBillingService } from "@/billing/billingService";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useSettingsNavigationLock } from "@/contexts/SettingsNavigationLockContext";
import { useBillingState } from "@/state/useLocalState";
import type { TeamStatus } from "@/types/team";
import { isIOS } from "@/utils/platform";
import {
  formatTeamSeatMismatchMessage,
  getTeamSeatCounts,
  getTeamSeatMismatch
} from "@/utils/teamSeats";
import { SettingsPage, SettingsSection } from "../SettingsPage";
import { TeamMembersSettings } from "./TeamMembersSettings";

function TeamSetup({ teamStatus }: { teamStatus: TeamStatus }) {
  const queryClient = useQueryClient();
  const [teamName, setTeamName] = useState("");
  const [isCreating, setIsCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useBlocker({
    shouldBlockFn: () => isCreating,
    disabled: !isCreating,
    enableBeforeUnload: isCreating
  });
  useSettingsNavigationLock(isCreating);

  const handleCreateTeam = async (event: React.FormEvent) => {
    event.preventDefault();

    if (!teamName.trim()) {
      setError("Please enter a team name");
      return;
    }

    setIsCreating(true);
    setError(null);

    try {
      await getBillingService().createTeam({ name: teamName.trim() });
      await queryClient.invalidateQueries({ queryKey: ["teamStatus"] });
    } catch (createError) {
      console.error("Failed to create team:", createError);
      setError(createError instanceof Error ? createError.message : "Failed to create team");
    } finally {
      setIsCreating(false);
    }
  };

  return (
    <SettingsPage
      title="Set up your team"
      description="Create your team to start inviting members and sharing plan usage."
    >
      <SettingsSection
        title="Team details"
        description="You can update the team name later from this page."
      >
        <form onSubmit={handleCreateTeam} className="max-w-xl space-y-5">
          <div className="grid gap-2">
            <Label htmlFor="settings-team-name">Team name</Label>
            <Input
              id="settings-team-name"
              value={teamName}
              onChange={(event) => setTeamName(event.target.value)}
              placeholder="e.g., My Company Team"
              disabled={isCreating}
              autoFocus
            />
            <p className="text-sm text-muted-foreground">
              This is how your team will be identified throughout Maple.
            </p>
          </div>

          {!!teamStatus.seats_purchased && (
            <Alert>
              <AlertCircle className="h-4 w-4" />
              <AlertDescription>
                Your team plan includes <strong>{teamStatus.seats_purchased} seats</strong>. You
                will be the team admin and can invite up to {teamStatus.seats_purchased - 1}{" "}
                additional members.
              </AlertDescription>
            </Alert>
          )}

          {error && (
            <Alert variant="destructive">
              <AlertCircle className="h-4 w-4" />
              <AlertDescription>{error}</AlertDescription>
            </Alert>
          )}

          <Button type="submit" disabled={isCreating}>
            {isCreating && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
            {isCreating ? "Creating team..." : "Create team"}
          </Button>
        </form>
      </SettingsSection>
    </SettingsPage>
  );
}

function TeamMemberDashboard({ teamStatus }: { teamStatus: TeamStatus }) {
  const queryClient = useQueryClient();
  const [showLeaveConfirmation, setShowLeaveConfirmation] = useState(false);
  const [isLeaving, setIsLeaving] = useState(false);
  const [leaveError, setLeaveError] = useState<string | null>(null);
  const seatMismatch = getTeamSeatMismatch(teamStatus);

  useBlocker({
    shouldBlockFn: () => isLeaving,
    disabled: !isLeaving,
    enableBeforeUnload: isLeaving
  });
  useSettingsNavigationLock(isLeaving);

  const handleLeaveTeam = async () => {
    setIsLeaving(true);
    setLeaveError(null);
    let shouldReload = false;

    try {
      await getBillingService().leaveTeam();
      await queryClient.invalidateQueries({ queryKey: ["teamStatus"] });
      shouldReload = true;
    } catch (error) {
      console.error("Failed to leave team:", error);
      setLeaveError(error instanceof Error ? error.message : "Failed to leave team");
    } finally {
      setIsLeaving(false);
    }

    if (shouldReload) {
      // Let the navigation blocker unregister before performing the intentional refresh.
      window.setTimeout(() => window.location.reload(), 0);
    }
  };

  return (
    <SettingsPage title="Team" description="View your team membership and shared plan access.">
      {seatMismatch && (
        <Alert variant="destructive">
          <AlertTriangle className="h-4 w-4" />
          <AlertDescription>
            <p className="font-medium">Team usage is paused</p>
            <p className="mt-1 text-sm">
              {formatTeamSeatMismatchMessage(seatMismatch, "member", !isIOS())}
            </p>
          </AlertDescription>
        </Alert>
      )}

      <SettingsSection title="Team information">
        <div className="flex min-w-0 items-start justify-between gap-3 rounded-lg bg-muted/50 p-4">
          <div className="min-w-0 flex-1">
            <p className="truncate text-lg font-semibold">{teamStatus.team_name}</p>
            {teamStatus.created_at && (
              <p className="mt-1 text-sm text-muted-foreground">
                Member since {new Date(teamStatus.created_at).toLocaleDateString()}
              </p>
            )}
          </div>
          <Badge variant="secondary" className="shrink-0">
            <User className="mr-1 h-3 w-3" />
            Member
          </Badge>
        </div>
      </SettingsSection>

      <SettingsSection
        title="Leave team"
        description="You will need another invitation if you want to rejoin this team."
        tone="danger"
      >
        {!showLeaveConfirmation ? (
          <Button
            type="button"
            variant="outline"
            className="text-destructive hover:text-destructive"
            onClick={() => {
              setLeaveError(null);
              setShowLeaveConfirmation(true);
            }}
          >
            <LogOut className="mr-2 h-4 w-4" />
            Leave team
          </Button>
        ) : (
          <div>
            <p className="text-sm font-medium">Leave team?</p>
            <p className="mt-1 text-sm leading-relaxed text-muted-foreground">
              Are you sure you want to leave the team? You will lose access to all team resources
              and will need to be invited again to rejoin.
            </p>
            {leaveError && (
              <Alert variant="destructive" className="mt-4">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription>{leaveError}</AlertDescription>
              </Alert>
            )}
            <div className="mt-5 flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
              <Button
                type="button"
                variant="outline"
                onClick={() => {
                  setShowLeaveConfirmation(false);
                  setLeaveError(null);
                }}
                disabled={isLeaving}
              >
                Cancel
              </Button>
              <Button
                type="button"
                variant="destructive"
                onClick={handleLeaveTeam}
                disabled={isLeaving}
              >
                {isLeaving && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                {isLeaving ? "Leaving..." : "Leave team"}
              </Button>
            </div>
          </div>
        )}
      </SettingsSection>
    </SettingsPage>
  );
}

function TeamAdminDashboard({ teamStatus }: { teamStatus: TeamStatus }) {
  const queryClient = useQueryClient();
  const { billingStatus } = useBillingState();
  const membersSectionRef = useRef<HTMLDivElement>(null);
  const [isEditingName, setIsEditingName] = useState(false);
  const [editedName, setEditedName] = useState("");
  const [isSavingName, setIsSavingName] = useState(false);
  const [nameError, setNameError] = useState<string | null>(null);
  const [isPortalLoading, setIsPortalLoading] = useState(false);
  const [portalError, setPortalError] = useState<string | null>(null);

  useBlocker({
    shouldBlockFn: () => isSavingName,
    disabled: !isSavingName,
    enableBeforeUnload: isSavingName
  });
  useSettingsNavigationLock(isSavingName);

  const seatCounts = getTeamSeatCounts(teamStatus);
  const seatMismatch = getTeamSeatMismatch(teamStatus);
  const seatsUsed = seatCounts.memberCount ?? 0;
  const seatsPurchased = seatCounts.billedSeatCount ?? 0;
  const seatUsagePercentage = seatsPurchased > 0 ? (seatsUsed / seatsPurchased) * 100 : 0;
  const canOpenBillingPortal = billingStatus
    ? !!billingStatus.stripe_customer_id
    : !!teamStatus.has_team_subscription;

  const startEditingName = () => {
    setEditedName(teamStatus.team_name ?? "");
    setNameError(null);
    setIsEditingName(true);
  };

  const cancelEditingName = () => {
    setEditedName("");
    setNameError(null);
    setIsEditingName(false);
  };

  const saveTeamName = async () => {
    const trimmedName = editedName.trim();

    if (!trimmedName) {
      setNameError("Team name cannot be empty");
      return;
    }

    if (trimmedName.length > 100) {
      setNameError("Team name must be 100 characters or less");
      return;
    }

    if (trimmedName === teamStatus.team_name) {
      cancelEditingName();
      return;
    }

    setIsSavingName(true);
    setNameError(null);

    try {
      await getBillingService().updateTeamName(trimmedName);
      await queryClient.invalidateQueries({ queryKey: ["teamStatus"] });
      await queryClient.invalidateQueries({ queryKey: ["billingStatus"] });
      cancelEditingName();
    } catch (error) {
      console.error("Failed to update team name:", error);
      setNameError(error instanceof Error ? error.message : "Failed to update team name");
    } finally {
      setIsSavingName(false);
    }
  };

  const handleOpenBilling = async () => {
    if (!canOpenBillingPortal) return;

    setPortalError(null);
    setIsPortalLoading(true);

    try {
      await openBillingPortal();
      await queryClient.invalidateQueries({ queryKey: ["billingStatus"] });
      await queryClient.invalidateQueries({ queryKey: ["teamStatus"] });
    } catch (error) {
      console.error("Failed to open billing portal:", error);
      setPortalError(
        "Unable to open subscription management. Please try again or contact support@trymaple.ai."
      );
    } finally {
      setIsPortalLoading(false);
    }
  };

  return (
    <SettingsPage
      title="Team"
      description="Manage your team, paid seats, members, and pending invitations."
    >
      {seatMismatch && (
        <Alert variant="destructive">
          <AlertTriangle className="h-4 w-4" />
          <AlertDescription>
            <p className="font-medium">Team usage is paused</p>
            <p className="mt-1 text-sm">
              {formatTeamSeatMismatchMessage(seatMismatch, "admin", !isIOS())}
            </p>
            <div className="mt-4 flex flex-col gap-2 lg:flex-row">
              <Button
                type="button"
                variant="outline"
                size="sm"
                className="border-destructive/30 text-destructive hover:text-destructive"
                onClick={() => membersSectionRef.current?.scrollIntoView({ block: "nearest" })}
              >
                <Users className="mr-1.5 h-3.5 w-3.5" />
                Manage members
              </Button>
              {canOpenBillingPortal && !isIOS() && (
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  className="border-destructive/30 text-destructive hover:text-destructive"
                  onClick={handleOpenBilling}
                  disabled={isPortalLoading}
                >
                  <CreditCard className="mr-1.5 h-3.5 w-3.5" />
                  {isPortalLoading ? "Opening..." : "Add seats"}
                </Button>
              )}
            </div>
            {portalError && <p className="mt-3 text-sm">{portalError}</p>}
          </AlertDescription>
        </Alert>
      )}

      <SettingsSection title="Team details">
        <div className="space-y-5">
          <div className="flex min-w-0 items-start justify-between gap-3">
            <div className="min-w-0 flex-1">
              {isEditingName ? (
                <div className="max-w-xl space-y-2">
                  <div className="flex min-w-0 flex-col gap-2 sm:flex-row sm:items-center">
                    <Input
                      value={editedName}
                      onChange={(event) => setEditedName(event.target.value)}
                      onKeyDown={(event) => {
                        if (event.key === "Enter") saveTeamName();
                        if (event.key === "Escape") cancelEditingName();
                      }}
                      maxLength={100}
                      autoFocus
                      disabled={isSavingName}
                      aria-label="Team name"
                      className="min-w-0"
                    />
                    <div className="flex shrink-0 justify-end gap-1">
                      {isSavingName ? (
                        <Loader2 className="m-3 h-4 w-4 shrink-0 animate-spin" />
                      ) : (
                        <>
                          <Button
                            type="button"
                            variant="ghost"
                            size="icon"
                            onClick={saveTeamName}
                            aria-label="Save team name"
                          >
                            <Check className="h-4 w-4" />
                          </Button>
                          <Button
                            type="button"
                            variant="ghost"
                            size="icon"
                            onClick={cancelEditingName}
                            aria-label="Cancel editing team name"
                          >
                            <X className="h-4 w-4" />
                          </Button>
                        </>
                      )}
                    </div>
                  </div>
                  <div className="flex flex-col gap-1 text-xs sm:flex-row sm:justify-between">
                    <span className="text-muted-foreground">
                      {editedName.trim().length}/100 characters
                    </span>
                    {nameError && <span className="text-destructive">{nameError}</span>}
                  </div>
                </div>
              ) : (
                <div className="flex min-w-0 items-center gap-2">
                  <p className="truncate text-lg font-semibold">{teamStatus.team_name}</p>
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    className="h-8 w-8 shrink-0"
                    onClick={startEditingName}
                    aria-label="Edit team name"
                  >
                    <Pencil className="h-3.5 w-3.5" />
                  </Button>
                </div>
              )}
              {!isEditingName && teamStatus.created_at && (
                <p className="mt-1 text-sm text-muted-foreground">
                  Created {new Date(teamStatus.created_at).toLocaleDateString()}
                </p>
              )}
            </div>
            {!isEditingName && (
              <Badge className="shrink-0">
                <Crown className="mr-1 h-3 w-3" />
                Admin
              </Badge>
            )}
          </div>

          <div className="space-y-2">
            <div className="flex items-center justify-between gap-3 text-sm">
              <span className="text-muted-foreground">Seat usage</span>
              <span className="font-medium">
                {seatsUsed}/{seatsPurchased} ({Math.round(seatUsagePercentage)}%)
              </span>
            </div>
            <div className="h-2 w-full overflow-hidden rounded-full bg-muted">
              <div
                className={
                  seatMismatch
                    ? "h-full bg-destructive transition-all"
                    : "h-full bg-emerald-500 transition-all"
                }
                style={{ width: `${Math.min(seatUsagePercentage, 100)}%` }}
              />
            </div>
          </div>

          <div className="flex flex-col gap-2 lg:flex-row">
            {seatMismatch ? (
              <Button type="button" disabled>
                <UserPlus className="mr-2 h-4 w-4" />
                Invite members
              </Button>
            ) : (
              <Button asChild>
                <Link to="/settings/team/invite">
                  <UserPlus className="mr-2 h-4 w-4" />
                  Invite members
                </Link>
              </Button>
            )}
            {canOpenBillingPortal && (
              <Button
                type="button"
                variant="outline"
                onClick={handleOpenBilling}
                disabled={isPortalLoading}
              >
                <CreditCard className="mr-2 h-4 w-4" />
                {isPortalLoading ? "Opening..." : "Manage subscription"}
              </Button>
            )}
          </div>
          {!seatMismatch && portalError && (
            <Alert variant="destructive">
              <AlertCircle className="h-4 w-4" />
              <AlertDescription>{portalError}</AlertDescription>
            </Alert>
          )}
        </div>
      </SettingsSection>

      <div ref={membersSectionRef}>
        <SettingsSection>
          <TeamMembersSettings teamStatus={teamStatus} />
        </SettingsSection>
      </div>
    </SettingsPage>
  );
}

export function TeamSettings() {
  const os = useOpenSecret();
  const {
    data: teamStatus,
    isLoading,
    isError,
    refetch
  } = useQuery<TeamStatus>({
    queryKey: ["teamStatus"],
    queryFn: () => getBillingService().getTeamStatus(),
    enabled: !!os.auth.user,
    refetchOnWindowFocus: true
  });

  if (isLoading || !teamStatus) {
    return (
      <SettingsPage title="Team" description="Manage your team, paid seats, and members.">
        {isError ? (
          <Alert variant="destructive">
            <AlertCircle className="h-4 w-4" />
            <AlertDescription className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
              <span>Unable to load your team information.</span>
              <Button type="button" variant="outline" size="sm" onClick={() => refetch()}>
                <RotateCw className="mr-2 h-3.5 w-3.5" />
                Try again
              </Button>
            </AlertDescription>
          </Alert>
        ) : (
          <SettingsSection>
            <div className="flex items-center justify-center py-10">
              <Loader2 className="h-6 w-6 animate-spin text-muted-foreground" />
            </div>
          </SettingsSection>
        )}
      </SettingsPage>
    );
  }

  if (teamStatus.has_team_subscription && !teamStatus.team_created) {
    return <TeamSetup teamStatus={teamStatus} />;
  }

  if (!teamStatus.team_created) {
    return (
      <SettingsPage title="Team" description="Team management is available with a Team plan.">
        <SettingsSection
          title="No team plan"
          description="Review available plans to create and manage a team in Maple."
        >
          <Button asChild>
            <Link to="/settings/billing">View billing settings</Link>
          </Button>
        </SettingsSection>
      </SettingsPage>
    );
  }

  const isAdmin = teamStatus.role === "admin" || teamStatus.is_team_admin === true;
  return isAdmin ? (
    <TeamAdminDashboard teamStatus={teamStatus} />
  ) : (
    <TeamMemberDashboard teamStatus={teamStatus} />
  );
}
