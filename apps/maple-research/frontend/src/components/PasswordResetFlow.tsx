import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { PasswordResetCompletion, PasswordResetOptions } from "@mapleai/sdk";
import { RecoveryCodeFormatError } from "@mapleai/sdk";
import { AmbiguousRecoveryCodeError, extractRecoveryCode } from "@/services/recoveryCodeInput";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";

type ResetApi = {
  getPasswordResetOptions: (
    email: string,
    code: string,
    secret: string
  ) => Promise<PasswordResetOptions>;
  completePasswordReset: (
    email: string,
    code: string,
    secret: string,
    password: string,
    completion: PasswordResetCompletion
  ) => Promise<void>;
};

// The backend is the validator: it looks the code up by a MAC of the exact
// string, so the form only removes what cannot be part of a code (whitespace
// picked up when copying from the email) and blocks empty input. Uppercasing is
// correct only while issued codes are uppercase-only.
function normalizeResetCode(input: string): string {
  return input.replace(/\s+/g, "").toUpperCase();
}

export function PasswordResetFlow({
  email,
  secret,
  api,
  onComplete,
  onRestart,
  onBusyChange
}: {
  email: string;
  secret: string;
  api: ResetApi;
  onComplete: () => void;
  onRestart: () => void;
  onBusyChange: (busy: boolean) => void;
}) {
  const [code, setCode] = useState("");
  const [options, setOptions] = useState<PasswordResetOptions | null>(null);
  const [mode, setMode] = useState<"preserve" | "destructive">("preserve");
  const [recoveryCode, setRecoveryCode] = useState("");
  const [showRecoveryCode, setShowRecoveryCode] = useState(false);
  const [newPassword, setNewPassword] = useState("");
  const [confirmation, setConfirmation] = useState("");
  const [acknowledged, setAcknowledged] = useState(false);
  const [busy, setBusy] = useState(false);
  const [success, setSuccess] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const alive = useRef(true);
  const inFlight = useRef(false);
  const generation = useRef(0);

  useEffect(() => {
    alive.current = true;
    const requestGeneration = ++generation.current;
    inFlight.current = false;
    setCode("");
    setOptions(null);
    setRecoveryCode("");
    setShowRecoveryCode(false);
    setNewPassword("");
    setConfirmation("");
    setAcknowledged(false);
    setError(null);
    setSuccess(false);
    setBusy(false);
    return () => {
      alive.current = false;
      generation.current = requestGeneration + 1;
    };
  }, [email, secret]);
  useLayoutEffect(() => {
    onBusyChange(busy);
    return () => onBusyChange(false);
  }, [busy, onBusyChange]);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (inFlight.current || !email || !secret || success) return;
    const emailCode = normalizeResetCode(code);
    if (!options && !emailCode) {
      setError("Enter the reset code from your email.");
      return;
    }
    if (options && ([...newPassword].length < 8 || [...newPassword].length > 64)) {
      setError("Use between 8 and 64 characters for your new password.");
      return;
    }
    if (options && newPassword !== confirmation) {
      setError("Make sure both passwords match.");
      return;
    }
    if (
      options &&
      ((mode === "preserve" && (!options.recovery_enrolled || !recoveryCode.trim())) ||
        (mode === "destructive" && (!options.destructive_reset_available || !acknowledged)))
    )
      return;
    const attempt = generation.current;
    const current = () => alive.current && attempt === generation.current;
    inFlight.current = true;
    setBusy(true);
    setError(null);
    try {
      if (!options) {
        const next = await api.getPasswordResetOptions(email, emailCode, secret);
        if (!current()) return;
        setCode(emailCode);
        setOptions(next);
        setMode(next.recovery_enrolled ? "preserve" : "destructive");
        setAcknowledged(false);
      } else {
        const completion: PasswordResetCompletion =
          mode === "preserve"
            ? { mode: "preserve", recoveryCode: await extractRecoveryCode(recoveryCode) }
            : { mode: "destructive", acknowledgeDataLoss: true };
        if (!current()) return;
        await api.completePasswordReset(email, code, secret, newPassword, completion);
        if (!current()) return;
        setCode("");
        setRecoveryCode("");
        setShowRecoveryCode(false);
        setNewPassword("");
        setConfirmation("");
        setOptions(null);
        setAcknowledged(false);
        setSuccess(true);
      }
    } catch (failure) {
      if (!current()) return;
      if (failure instanceof RecoveryCodeFormatError) {
        setError(
          failure instanceof AmbiguousRecoveryCodeError
            ? failure.message
            : "Enter a valid Maple recovery code. You can paste the code or the saved text file's contents."
        );
        return;
      }
      setError(
        options
          ? "We couldn't confirm the reset. If it completed, sign in with your new password. Otherwise, request a new reset email before trying again; an incorrect recovery code can use up the reset request."
          : "Couldn't verify this reset request. Check the email code or request a new email."
      );
      if ((failure as { status?: number } | null)?.status === 400) {
        setOptions(null);
        setCode("");
        setRecoveryCode("");
        setShowRecoveryCode(false);
        setNewPassword("");
        setConfirmation("");
        setAcknowledged(false);
      }
    } finally {
      if (current()) {
        inFlight.current = false;
        setBusy(false);
      }
    }
  }

  if (!email || !secret)
    return (
      <Alert variant="destructive" role="alert">
        <AlertDescription>
          This reset request is incomplete. Request a new email to continue.
        </AlertDescription>
        <Button variant="outline" onClick={onRestart}>
          Request a new reset email
        </Button>
      </Alert>
    );
  if (success)
    return (
      <Card className="bg-card/70 backdrop-blur-sm mx-auto max-w-[45rem]">
        <CardHeader>
          <CardTitle className="text-xl">Password reset successful</CardTitle>
          <CardDescription>Your password is updated and you are signed in.</CardDescription>
        </CardHeader>
        <CardContent>
          <Button className="w-full" onClick={onComplete}>
            Continue to Maple
          </Button>
        </CardContent>
      </Card>
    );

  return (
    <Card className="bg-card/70 backdrop-blur-sm mx-auto max-w-[45rem]">
      <CardHeader>
        <CardTitle className="text-xl">
          {options ? "Choose how to reset your password" : "Confirm password reset"}
        </CardTitle>
        <CardDescription>
          {options
            ? "Protect your encrypted data with your saved recovery code."
            : "Enter the code from your reset email. We'll check which reset options are available."}
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form onSubmit={submit} className="grid gap-4">
          {error && (
            <Alert variant="destructive" role="alert">
              <AlertDescription>{error}</AlertDescription>
            </Alert>
          )}
          <fieldset disabled={busy} className="grid min-w-0 gap-4">
            {!options ? (
              <div className="grid gap-2">
                <Label htmlFor="reset-email-code">Email reset code</Label>
                <Input
                  id="reset-email-code"
                  value={code}
                  onChange={(event) => setCode(event.target.value)}
                  autoComplete="one-time-code"
                  autoCapitalize="characters"
                  spellCheck={false}
                  required
                />
                <p className="text-sm text-muted-foreground">
                  Check spam if the email hasn't arrived. Accounts created with Apple, Google, or
                  GitHub may use that sign-in method instead.
                </p>
              </div>
            ) : (
              <>
                <fieldset className="grid gap-3">
                  <legend className="mb-2 text-sm font-medium">Reset method</legend>
                  {options.recovery_enrolled && (
                    <label className="flex items-start gap-3 rounded-lg border p-3 text-sm">
                      <input
                        type="radio"
                        name="reset-mode"
                        value="preserve"
                        checked={mode === "preserve"}
                        onChange={() => {
                          setMode("preserve");
                          setAcknowledged(false);
                        }}
                        className="mt-1"
                      />
                      <span>
                        <span className="block font-medium">Use my recovery code</span>
                        <span className="text-muted-foreground">
                          Keep your chats and encrypted data. Your recovery code will remain valid.
                        </span>
                      </span>
                    </label>
                  )}
                  {options.destructive_reset_available && (
                    <label className="flex items-start gap-3 rounded-lg border border-destructive/40 p-3 text-sm">
                      <input
                        type="radio"
                        name="reset-mode"
                        value="destructive"
                        checked={mode === "destructive"}
                        onChange={() => {
                          setMode("destructive");
                          setRecoveryCode("");
                          setShowRecoveryCode(false);
                          setAcknowledged(false);
                        }}
                        className="mt-1"
                      />
                      <span>
                        <span className="block font-medium">Reset and delete encrypted data</span>
                        <span className="text-muted-foreground">
                          Use this only if you cannot recover your account with a saved code.
                        </span>
                      </span>
                    </label>
                  )}
                </fieldset>
                {!options.recovery_enrolled && (
                  <p className="text-sm text-muted-foreground">
                    No recovery code is enabled for this account.
                  </p>
                )}
                {!options.recovery_enrolled && !options.destructive_reset_available ? (
                  <Alert role="alert">
                    <AlertDescription>
                      No reset method is currently available. Request a new email or contact
                      support.
                    </AlertDescription>
                  </Alert>
                ) : (
                  <>
                    {mode === "preserve" ? (
                      <div className="grid gap-2">
                        <Label htmlFor="reset-recovery-code">Recovery code</Label>
                        <Input
                          id="reset-recovery-code"
                          type={showRecoveryCode ? "text" : "password"}
                          autoComplete="off"
                          spellCheck={false}
                          value={recoveryCode}
                          onChange={(event) => setRecoveryCode(event.target.value)}
                          required
                        />
                        <Button
                          type="button"
                          variant="ghost"
                          aria-controls="reset-recovery-code"
                          aria-pressed={showRecoveryCode}
                          onClick={() => setShowRecoveryCode((value) => !value)}
                        >
                          {showRecoveryCode ? "Hide recovery code" : "Show recovery code"}
                        </Button>
                        <p className="text-sm text-muted-foreground">
                          Use the code you saved in Security settings. An incorrect code can use up
                          this email reset request.
                        </p>
                      </div>
                    ) : (
                      <Alert variant="destructive">
                        <AlertDescription>
                          <p className="mb-3">
                            This permanently deletes your encrypted chats and saved data, removes
                            any recovery code, and disconnects linked sign-in providers. Your
                            existing chats cannot be restored afterward.
                          </p>
                          <label className="flex items-start gap-2">
                            <input
                              type="checkbox"
                              checked={acknowledged}
                              onChange={(event) => setAcknowledged(event.target.checked)}
                              required
                              className="mt-1"
                            />
                            <span>
                              I understand that this reset permanently deletes my encrypted data.
                            </span>
                          </label>
                        </AlertDescription>
                      </Alert>
                    )}
                    <div className="grid gap-2">
                      <Label htmlFor="reset-new-password">New password</Label>
                      <Input
                        id="reset-new-password"
                        type="password"
                        autoComplete="new-password"
                        value={newPassword}
                        onChange={(event) => setNewPassword(event.target.value)}
                        required
                        aria-describedby="reset-password-policy"
                      />
                      <p id="reset-password-policy" className="text-sm text-muted-foreground">
                        Use 8–64 characters.
                      </p>
                    </div>
                    <div className="grid gap-2">
                      <Label htmlFor="reset-confirm-password">Confirm new password</Label>
                      <Input
                        id="reset-confirm-password"
                        type="password"
                        autoComplete="new-password"
                        value={confirmation}
                        onChange={(event) => setConfirmation(event.target.value)}
                        required
                      />
                    </div>
                  </>
                )}
              </>
            )}
          </fieldset>
          <Button
            type="submit"
            className="w-full"
            variant={options && mode === "destructive" ? "destructive" : "default"}
            disabled={
              busy ||
              (!!options &&
                (mode === "preserve"
                  ? !options.recovery_enrolled || !recoveryCode.trim()
                  : !options.destructive_reset_available || !acknowledged))
            }
          >
            {busy
              ? "Please wait..."
              : !options
                ? "Verify email code"
                : mode === "preserve"
                  ? "Reset password and keep data"
                  : "Reset password and delete data"}
          </Button>
          <Button type="button" variant="ghost" disabled={busy} onClick={onRestart}>
            Request a new reset email
          </Button>
        </form>
      </CardContent>
    </Card>
  );
}
