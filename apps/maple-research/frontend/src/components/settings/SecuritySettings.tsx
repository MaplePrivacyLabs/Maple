import { useState } from "react";
import { Link, useBlocker } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useSettingsNavigationLock } from "@/contexts/SettingsNavigationLockContext";
import { SettingsPage, SettingsSection } from "./SettingsPage";

export function SecuritySettings() {
  const os = useOpenSecret();
  const [currentPassword, setCurrentPassword] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");
  const [isLoading, setIsLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState(false);

  useBlocker({
    shouldBlockFn: () => isLoading,
    disabled: !isLoading,
    enableBeforeUnload: isLoading
  });
  useSettingsNavigationLock(isLoading);

  const loginMethod = os.auth.user?.user.login_method?.toLowerCase();
  const canChangePassword = loginMethod === "email" || loginMethod === "guest";

  const handleSubmit = async (event: React.FormEvent) => {
    event.preventDefault();
    setError(null);
    setSuccess(false);

    if (newPassword !== confirmPassword) {
      setError(
        "Passwords do not match. Please make sure your new password and confirmation match."
      );
      return;
    }

    setIsLoading(true);
    try {
      await os.changePassword(currentPassword, newPassword);
      setSuccess(true);
      setCurrentPassword("");
      setNewPassword("");
      setConfirmPassword("");
    } catch (changeError) {
      console.error("Failed to change password:", changeError);
      setError("Failed to change password. Please check your current password and try again.");
    } finally {
      setIsLoading(false);
    }
  };

  return (
    <SettingsPage title="Security" description="Manage how you sign in to your Maple account.">
      <SettingsSection
        title="Change password"
        description="Use at least eight characters for your new password."
      >
        {canChangePassword ? (
          <form onSubmit={handleSubmit} className="space-y-4">
            {error && (
              <Alert variant="destructive">
                <AlertDescription>{error}</AlertDescription>
              </Alert>
            )}
            {success && (
              <Alert>
                <AlertDescription>Password changed successfully.</AlertDescription>
              </Alert>
            )}
            <div className="grid gap-2">
              <Label htmlFor="settings-current-password">Current password</Label>
              <Input
                id="settings-current-password"
                type="password"
                value={currentPassword}
                onChange={(event) => {
                  setCurrentPassword(event.target.value);
                  setError(null);
                  setSuccess(false);
                }}
                required
                autoComplete="current-password"
              />
            </div>
            <div className="grid gap-2 sm:grid-cols-2 sm:gap-4">
              <div className="grid gap-2">
                <Label htmlFor="settings-new-password">New password</Label>
                <Input
                  id="settings-new-password"
                  type="password"
                  value={newPassword}
                  onChange={(event) => {
                    setNewPassword(event.target.value);
                    setError(null);
                    setSuccess(false);
                  }}
                  required
                  minLength={8}
                  autoComplete="new-password"
                />
              </div>
              <div className="grid gap-2">
                <Label htmlFor="settings-confirm-password">Confirm new password</Label>
                <Input
                  id="settings-confirm-password"
                  type="password"
                  value={confirmPassword}
                  onChange={(event) => {
                    setConfirmPassword(event.target.value);
                    setError(null);
                    setSuccess(false);
                  }}
                  required
                  minLength={8}
                  autoComplete="new-password"
                />
              </div>
            </div>
            <div className="flex justify-end">
              <Button type="submit" disabled={isLoading || success}>
                {isLoading ? "Changing password..." : "Change password"}
              </Button>
            </div>
          </form>
        ) : (
          <div className="space-y-4">
            <p className="text-sm text-muted-foreground">
              This account signs in through an external provider, so it does not have a Maple
              password to change.
            </p>
            <Button asChild variant="outline">
              <Link to="/settings/account" replace>
                Back to account
              </Link>
            </Button>
          </div>
        )}
      </SettingsSection>
    </SettingsPage>
  );
}
