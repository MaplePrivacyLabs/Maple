import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import {
  CheckCircle,
  ChevronRight,
  MessageSquareText,
  Monitor,
  Moon,
  ShieldCheck,
  Sun,
  Trash2,
  XCircle
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useTheme } from "@/contexts/ThemeContext";
import { useBillingState } from "@/state/useLocalState";
import { planDisplayName } from "@/billing/plan";
import { SettingsPage, SettingsSection } from "./SettingsPage";

type SettingsLinkRowProps = {
  to: "/settings/preferences" | "/settings/security" | "/settings/delete-account";
  title: string;
  description: string;
  icon: typeof MessageSquareText;
  danger?: boolean;
};

function SettingsLinkRow({ to, title, description, icon: Icon, danger }: SettingsLinkRowProps) {
  return (
    <Link
      to={to}
      replace
      className="flex items-center gap-3 rounded-lg border border-border/70 p-3 transition-colors hover:border-[hsl(var(--maple-primary))]/60 hover:bg-muted/50 sm:p-4"
    >
      <div
        className={`flex h-9 w-9 shrink-0 items-center justify-center rounded-lg ${
          danger ? "bg-destructive/10 text-destructive" : "bg-muted text-foreground"
        }`}
      >
        <Icon className="h-4 w-4" />
      </div>
      <div className="min-w-0 flex-1">
        <p className={danger ? "text-sm font-medium text-destructive" : "text-sm font-medium"}>
          {title}
        </p>
        <p className="mt-0.5 text-xs leading-relaxed text-muted-foreground">{description}</p>
      </div>
      <ChevronRight className="h-4 w-4 shrink-0 text-muted-foreground" />
    </Link>
  );
}

export function AccountSettings() {
  const os = useOpenSecret();
  const { billingStatus } = useBillingState();
  const { theme, setTheme } = useTheme();
  const [verificationStatus, setVerificationStatus] = useState<"unverified" | "pending">(
    "unverified"
  );

  const user = os.auth.user?.user;
  const isEmailUser = user?.login_method === "email";
  const isGuestUser = user?.login_method?.toLowerCase() === "guest";

  const handleResendVerification = async () => {
    try {
      await os.requestNewVerificationEmail();
      setVerificationStatus("pending");
    } catch (error) {
      console.error("Failed to resend verification email:", error);
    }
  };

  const periodLabel =
    billingStatus?.payment_provider === "subscription_pass" ||
    billingStatus?.payment_provider === "zaprite"
      ? "Expires"
      : "Renews";

  return (
    <SettingsPage
      title="Account"
      description="Manage your profile, appearance, and personal account settings."
    >
      <SettingsSection title="Profile" description="Your Maple identity and current plan.">
        <div className="grid gap-5">
          {!isGuestUser && (
            <div className="grid gap-2">
              <Label htmlFor="settings-email">Email</Label>
              <div className="flex items-center gap-2">
                <Input
                  id="settings-email"
                  type="email"
                  value={user?.email ?? ""}
                  disabled
                  className="min-w-0"
                />
                {user?.email_verified ? (
                  <CheckCircle
                    className="h-5 w-5 shrink-0 text-maple-success"
                    aria-label="Verified"
                  />
                ) : (
                  <XCircle className="h-5 w-5 shrink-0 text-maple-error" aria-label="Unverified" />
                )}
              </div>
              {!user?.email_verified && (
                <p className="text-sm text-muted-foreground">
                  {verificationStatus === "unverified" ? (
                    <>
                      Unverified —{" "}
                      <button
                        type="button"
                        onClick={handleResendVerification}
                        className="text-[hsl(var(--maple-primary-strong))] hover:underline"
                      >
                        resend verification email
                      </button>
                    </>
                  ) : (
                    "Verification email sent. Check your inbox."
                  )}
                </p>
              )}
            </div>
          )}

          <div className="grid gap-2">
            <Label>Plan</Label>
            <div className="rounded-lg border border-border/70 bg-muted/40 px-3 py-2.5">
              <p className="text-sm font-medium">
                {billingStatus ? `${planDisplayName(billingStatus)} Plan` : "Loading..."}
              </p>
              {billingStatus?.current_period_end && (
                <p className="mt-0.5 text-xs text-muted-foreground">
                  {periodLabel} on{" "}
                  {new Date(Number(billingStatus.current_period_end) * 1000).toLocaleDateString(
                    undefined,
                    { year: "numeric", month: "long", day: "numeric" }
                  )}
                </p>
              )}
            </div>
          </div>
        </div>
      </SettingsSection>

      <SettingsSection title="Appearance" description="Choose how Maple looks on this device.">
        <div className="grid grid-cols-1 gap-2 sm:grid-cols-3">
          <Button
            type="button"
            variant={theme === "light" ? "primary" : "outline"}
            onClick={() => setTheme("light")}
          >
            <Sun className="mr-2 h-4 w-4" />
            Light
          </Button>
          <Button
            type="button"
            variant={theme === "dark" ? "primary" : "outline"}
            onClick={() => setTheme("dark")}
          >
            <Moon className="mr-2 h-4 w-4" />
            Dark
          </Button>
          <Button
            type="button"
            variant={theme === "system" ? "primary" : "outline"}
            onClick={() => setTheme("system")}
          >
            <Monitor className="mr-2 h-4 w-4" />
            System
          </Button>
        </div>
      </SettingsSection>

      <SettingsSection title="More account settings">
        <div className="space-y-2">
          <SettingsLinkRow
            to="/settings/preferences"
            title="User preferences"
            description="Set the default system prompt used for your conversations."
            icon={MessageSquareText}
          />
          {(isEmailUser || isGuestUser) && (
            <SettingsLinkRow
              to="/settings/security"
              title="Change password"
              description="Update the password used to sign in to this account."
              icon={ShieldCheck}
            />
          )}
          <SettingsLinkRow
            to="/settings/delete-account"
            title="Delete account"
            description="Permanently delete your account and all associated data."
            icon={Trash2}
            danger
          />
        </div>
      </SettingsSection>
    </SettingsPage>
  );
}
