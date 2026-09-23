import { useState } from "react";
import { Link, useBlocker } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useOpenSecret } from "@mapleai/sdk";
import {
  AlertCircle,
  ArrowLeft,
  CreditCard,
  Info,
  Loader2,
  RotateCw,
  UserPlus
} from "lucide-react";
import { openBillingPortal } from "@/billing/billingPortal";
import { getBillingService } from "@/billing/billingService";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import {
  useSettingsNavigationLock,
  useSettingsNavigationLockState
} from "@/contexts/SettingsNavigationLockContext";
import { useBillingState } from "@/state/useLocalState";
import type { TeamStatus } from "@/types/team";
import { isIOS } from "@/utils/platform";
import { getTeamSeatMismatch } from "@/utils/teamSeats";
import { SettingsPage, SettingsSection } from "../SettingsPage";

function BackToTeamButton() {
  const isNavigationLocked = useSettingsNavigationLockState();

  if (isNavigationLocked) {
    return (
      <Button type="button" variant="outline" size="sm" disabled>
        <ArrowLeft className="mr-2 h-4 w-4" />
        Back to team
      </Button>
    );
  }

  return (
    <Button asChild variant="outline" size="sm">
      <Link to="/settings/team">
        <ArrowLeft className="mr-2 h-4 w-4" />
        Back to team
      </Link>
    </Button>
  );
}

export function TeamInviteSettings() {
  const os = useOpenSecret();
  const queryClient = useQueryClient();
  const { billingStatus } = useBillingState();
  const [emails, setEmails] = useState("");
  const [isInviting, setIsInviting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [successMessage, setSuccessMessage] = useState<string | null>(null);
  const [isPortalLoading, setIsPortalLoading] = useState(false);

  useBlocker({
    shouldBlockFn: () => isInviting,
    disabled: !isInviting,
    enableBeforeUnload: isInviting
  });
  useSettingsNavigationLock(isInviting);
  const isNavigationLocked = useSettingsNavigationLockState();

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
      <SettingsPage
        title="Invite team members"
        description="Send invitations to people who should join your Maple team."
        actions={<BackToTeamButton />}
      >
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

  const isAdmin = teamStatus.role === "admin" || teamStatus.is_team_admin === true;
  const seatMismatch = getTeamSeatMismatch(teamStatus);
  const seatsAvailable = Math.max(0, teamStatus.seats_available ?? 0);
  const canOpenBillingPortal = billingStatus
    ? !!billingStatus.stripe_customer_id
    : !!teamStatus.has_team_subscription;

  const handleManageSubscription = async () => {
    if (!canOpenBillingPortal) return;

    setError(null);
    setIsPortalLoading(true);

    try {
      await openBillingPortal();
      await queryClient.invalidateQueries({ queryKey: ["billingStatus"] });
      await queryClient.invalidateQueries({ queryKey: ["teamStatus"] });
    } catch (portalError) {
      console.error("Failed to open billing portal:", portalError);
      setError(
        "Unable to open subscription management. Please try again or contact support@trymaple.ai."
      );
    } finally {
      setIsPortalLoading(false);
    }
  };

  const handleInvite = async (event: React.FormEvent) => {
    event.preventDefault();

    const emailList = emails
      .split(/[\n,]/)
      .map((email) => email.trim())
      .filter((email) => email.length > 0);

    if (emailList.length === 0) {
      setError("Please enter at least one email address");
      return;
    }

    const emailRegex = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
    const invalidEmails = emailList.filter((email) => !emailRegex.test(email));

    if (invalidEmails.length > 0) {
      setError(`Invalid email format: ${invalidEmails.join(", ")}`);
      return;
    }

    if (emailList.length > seatsAvailable) {
      setError(
        `Cannot invite ${emailList.length} members. Only ${seatsAvailable} ${
          seatsAvailable === 1 ? "seat is" : "seats are"
        } available.`
      );
      return;
    }

    setIsInviting(true);
    setError(null);
    setSuccessMessage(null);

    try {
      const response = await getBillingService().inviteTeamMembers({ emails: emailList });
      await queryClient.invalidateQueries({ queryKey: ["teamStatus"] });
      await queryClient.invalidateQueries({ queryKey: ["teamMembers"] });

      const inviteCount = response.invites.length;
      setSuccessMessage(
        `Successfully sent ${inviteCount} ${inviteCount === 1 ? "invite" : "invites"}`
      );
      setEmails("");
    } catch (inviteError) {
      console.error("Failed to invite members:", inviteError);
      setError(inviteError instanceof Error ? inviteError.message : "Failed to send invites");
    } finally {
      setIsInviting(false);
    }
  };

  if (!teamStatus.team_created || !isAdmin) {
    return (
      <SettingsPage
        title="Invite team members"
        description="Only the team admin can invite members."
        actions={<BackToTeamButton />}
      >
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>
            {!teamStatus.team_created
              ? "Set up your team before inviting members."
              : "You must be a team admin to send invitations."}
          </AlertDescription>
        </Alert>
      </SettingsPage>
    );
  }

  if (seatMismatch) {
    return (
      <SettingsPage
        title="Invite team members"
        description="Resolve your paid seat count before sending more invitations."
        actions={<BackToTeamButton />}
      >
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>
            {isIOS()
              ? "Team usage is paused while the team has more members than paid seats. Remove members before inviting anyone else."
              : "Team usage is paused while the team has more members than paid seats. Add seats or remove members before inviting anyone else."}
          </AlertDescription>
        </Alert>
        {canOpenBillingPortal && (
          <Button
            type="button"
            variant="outline"
            onClick={handleManageSubscription}
            disabled={isPortalLoading}
          >
            <CreditCard className="mr-2 h-4 w-4" />
            {isPortalLoading ? "Opening..." : "Manage subscription"}
          </Button>
        )}
        {error && (
          <Alert variant="destructive">
            <AlertCircle className="h-4 w-4" />
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}
      </SettingsPage>
    );
  }

  return (
    <SettingsPage
      title="Invite team members"
      description="Send invitations to people who should join your Maple team."
      actions={<BackToTeamButton />}
    >
      <SettingsSection
        title="Email addresses"
        description="Invite one person or several people in the same request."
      >
        <form onSubmit={handleInvite} className="space-y-5">
          <div className="grid gap-2">
            <Label htmlFor="settings-team-invite-emails">Email addresses</Label>
            <Textarea
              id="settings-team-invite-emails"
              value={emails}
              onChange={(event) => setEmails(event.target.value)}
              placeholder={`john@example.com\njane@example.com\nor comma-separated: john@example.com, jane@example.com`}
              disabled={isInviting}
              rows={6}
              className="resize-none"
            />
            <p className="text-sm text-muted-foreground">
              Enter one email per line or separate addresses with commas.
            </p>
          </div>

          {seatsAvailable > 0 && (
            <Alert>
              <Info className="h-4 w-4" />
              <AlertDescription>
                You have <strong>{seatsAvailable}</strong> available{" "}
                {seatsAvailable === 1 ? "seat" : "seats"}.
              </AlertDescription>
            </Alert>
          )}

          {seatsAvailable === 0 && !successMessage && !isInviting && (
            <div className="space-y-3">
              <Alert variant="destructive">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription>
                  {isIOS()
                    ? "No seats are available. Remove existing members before inviting new ones."
                    : "No seats are available. Purchase additional seats or remove existing members before inviting new ones."}
                </AlertDescription>
              </Alert>
              {canOpenBillingPortal && (
                <Button
                  type="button"
                  variant="outline"
                  onClick={handleManageSubscription}
                  disabled={isPortalLoading}
                >
                  <CreditCard className="mr-2 h-4 w-4" />
                  {isPortalLoading ? "Opening..." : "Manage subscription"}
                </Button>
              )}
            </div>
          )}

          {error && (
            <Alert variant="destructive">
              <AlertCircle className="h-4 w-4" />
              <AlertDescription>{error}</AlertDescription>
            </Alert>
          )}

          {successMessage && (
            <Alert className="border-maple-success/30 bg-maple-success/10">
              <UserPlus className="h-4 w-4 text-maple-success" />
              <AlertDescription className="text-maple-success">{successMessage}</AlertDescription>
            </Alert>
          )}

          <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
            {isNavigationLocked ? (
              <Button type="button" variant="outline" disabled>
                Cancel
              </Button>
            ) : (
              <Button asChild type="button" variant="outline">
                <Link to="/settings/team">Cancel</Link>
              </Button>
            )}
            <Button type="submit" disabled={isInviting || seatsAvailable === 0}>
              {isInviting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              {isInviting ? "Sending invites..." : "Send invites"}
            </Button>
          </div>
        </form>
      </SettingsSection>
    </SettingsPage>
  );
}
