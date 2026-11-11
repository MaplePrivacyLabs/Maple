import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { RecoveryCodeResponse, RecoveryCodeStatus } from "@mapleai/sdk";
import { normalizeRecoveryCode } from "@mapleai/sdk";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { SettingsSection } from "./SettingsPage";
import { saveRecoveryCode } from "@/services/recoveryCodeExport";
import { AmbiguousRecoveryCodeError, extractRecoveryCode } from "@/services/recoveryCodeInput";
import { isTauriMobile } from "@/utils/platform";

type RecoveryApi = {
  getRecoveryCodeStatus: () => Promise<RecoveryCodeStatus>;
  enrollRecoveryCode: (password: string) => Promise<RecoveryCodeResponse>;
  rotateRecoveryCode: (password: string) => Promise<RecoveryCodeResponse>;
  disableRecoveryCode: (password: string) => Promise<void>;
};
type Action = "enable" | "replace" | "disable";

/** Mount with an account-specific key: secrets and late responses belong to that account only. */
export function RecoveryCodeSettings({
  api,
  onLockChange,
  saveCode = saveRecoveryCode,
  mobile = isTauriMobile()
}: {
  api: RecoveryApi;
  onLockChange: (locked: boolean) => void;
  saveCode?: typeof saveRecoveryCode;
  mobile?: boolean;
}) {
  const [status, setStatus] = useState<RecoveryCodeStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [action, setAction] = useState<Action | null>(null);
  const [password, setPassword] = useState("");
  const [code, setCode] = useState<string | null>(null);
  const [confirmation, setConfirmation] = useState("");
  const [showConfirmation, setShowConfirmation] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const confirmInFlight = useRef(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const saveInFlight = useRef(false);
  const alive = useRef(true);
  const inFlight = useRef(false);
  const statusRevision = useRef(0);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  useLayoutEffect(() => {
    onLockChange(busy || code !== null);
    return () => onLockChange(false);
  }, [busy, code, onLockChange]);

  const fetchStatus = useCallback(async () => api.getRecoveryCodeStatus(), [api]);
  useEffect(() => {
    let cancelled = false;
    const revision = ++statusRevision.current;
    setLoading(true);
    fetchStatus().then(
      (next) => {
        if (!cancelled && revision === statusRevision.current) {
          setStatus(next);
          setLoading(false);
        }
      },
      () => {
        if (!cancelled && revision === statusRevision.current) {
          setStatus(null);
          setLoading(false);
        }
      }
    );
    return () => {
      cancelled = true;
    };
  }, [fetchStatus]);

  async function refresh() {
    const revision = ++statusRevision.current;
    setLoading(true);
    try {
      const next = await fetchStatus();
      if (alive.current && revision === statusRevision.current) setStatus(next);
    } catch {
      if (alive.current && revision === statusRevision.current) setStatus(null);
    } finally {
      if (alive.current && revision === statusRevision.current) setLoading(false);
    }
  }

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (!action || !password || inFlight.current) return;
    inFlight.current = true;
    ++statusRevision.current;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      if (action === "disable") {
        await api.disableRecoveryCode(password);
        if (!alive.current) return;
        ++statusRevision.current;
        setStatus({ enrolled: false, issued_at: null, last_used_at: null, use_count: 0 });
        setNotice("Recovery code disabled. Your previous code no longer works.");
      } else {
        const result = await (action === "enable"
          ? api.enrollRecoveryCode(password)
          : api.rotateRecoveryCode(password));
        if (!alive.current) return;
        ++statusRevision.current;
        setCode(result.recovery_code);
        setConfirmation("");
        setShowConfirmation(false);
        setStatus({ enrolled: true, issued_at: null, last_used_at: null, use_count: 0 });
      }
      setAction(null);
    } catch (failure) {
      if (!alive.current) return;
      ++statusRevision.current;
      const httpStatus = (failure as { status?: number } | null)?.status;
      setError(
        httpStatus === 401
          ? "Unable to confirm your password. Check your password and sign-in, then try again."
          : "We couldn't confirm the change. Refresh recovery status before trying again. If recovery is enabled but you didn't receive a code, replace it with a new one."
      );
      // A lost response can follow a committed enrollment/rotation. Never retry automatically.
      if (httpStatus !== 401) {
        setStatus(null);
        setAction(null);
      }
    } finally {
      if (alive.current) {
        setPassword("");
        setBusy(false);
        setLoading(false);
      }
      inFlight.current = false;
    }
  }

  async function copyCode() {
    if (!code) return;
    try {
      await navigator.clipboard.writeText(code);
      if (alive.current) {
        setNotice("Code copied. Store it somewhere safe before continuing.");
        setError(null);
      }
    } catch {
      if (alive.current)
        setError(
          "Couldn't copy the code. Select it and copy it manually, then store it somewhere safe."
        );
    }
  }

  async function downloadCode() {
    if (!code || saveInFlight.current) return;
    saveInFlight.current = true;
    setSaving(true);
    setError(null);
    setNotice(null);
    try {
      const result = await saveCode(code);
      if (!alive.current) return;
      if (result === "saved") setNotice("Recovery code file saved. Store it somewhere safe.");
      if (result === "downloaded") setNotice("Download started. Check that the file was saved.");
    } catch {
      if (alive.current)
        setError("Couldn't save a file. Copy the code and store it somewhere safe instead.");
    } finally {
      saveInFlight.current = false;
      if (alive.current) setSaving(false);
    }
  }

  async function confirmSavedCode() {
    if (!code || !confirmation.trim() || saveInFlight.current || confirmInFlight.current) return;
    confirmInFlight.current = true;
    setConfirming(true);
    setError(null);
    try {
      const savedCode = await extractRecoveryCode(confirmation);
      if (!alive.current) return;
      if (savedCode !== normalizeRecoveryCode(code)) {
        setError("That code doesn't match. Paste the recovery code you just saved and try again.");
        return;
      }
      setCode(null);
      setConfirmation("");
      setShowConfirmation(false);
      setNotice("Recovery is enabled. Keep your saved code private.");
      void refresh();
    } catch (failure) {
      if (alive.current)
        setError(
          failure instanceof AmbiguousRecoveryCodeError
            ? failure.message
            : "That code doesn't match. Paste the recovery code you just saved and try again."
        );
    } finally {
      confirmInFlight.current = false;
      if (alive.current) setConfirming(false);
    }
  }

  return (
    <SettingsSection
      title={code ? "Save your recovery code" : "Recovery code"}
      description="Reset your password without losing your encrypted data. Recovery requires your code and access to your account email. Keep the code somewhere safe, such as a password manager."
    >
      <div className="space-y-4">
        {error && (
          <Alert variant="destructive" role="alert">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}
        {notice && (
          <p role="status" className="text-sm text-muted-foreground">
            {notice}
          </p>
        )}
        {code ? (
          <>
            <Alert>
              <AlertDescription>
                This is the only time Maple will show this code. If you replaced a code, the
                previous one no longer works. We cannot email you another copy.
              </AlertDescription>
            </Alert>
            <div className="grid gap-2">
              <Label htmlFor="recovery-code">Your recovery code</Label>
              <textarea
                id="recovery-code"
                readOnly
                value={code}
                rows={3}
                spellCheck={false}
                autoComplete="off"
                className="w-full resize-none rounded-md border border-input bg-background p-3 font-mono text-sm break-all"
              />
            </div>
            <div className="flex flex-wrap gap-2">
              <Button variant="outline" onClick={() => void copyCode()}>
                Copy code
              </Button>
              {!mobile && (
                <Button
                  variant="outline"
                  onClick={() => downloadCode()}
                  disabled={saving || confirming}
                >
                  {saving ? "Saving…" : "Save text file"}
                </Button>
              )}
            </div>
            <div className="grid gap-2">
              <Label htmlFor="recovery-confirmation">Confirm your saved recovery code</Label>
              <Input
                id="recovery-confirmation"
                type={showConfirmation ? "text" : "password"}
                value={confirmation}
                disabled={confirming}
                onChange={(event) => {
                  setConfirmation(event.target.value);
                  setError(null);
                }}
                autoComplete="off"
                spellCheck={false}
                aria-describedby="recovery-confirmation-help"
              />
              <p id="recovery-confirmation-help" className="text-sm text-muted-foreground">
                Paste the code from where you saved it. You can also paste the saved text file's
                contents.
              </p>
              <Button
                type="button"
                variant="ghost"
                aria-controls="recovery-confirmation"
                aria-pressed={showConfirmation}
                onClick={() => setShowConfirmation((value) => !value)}
              >
                {showConfirmation ? "Hide saved code" : "Show saved code"}
              </Button>
            </div>
            <div className="flex justify-end">
              <Button
                disabled={!confirmation.trim() || saving || confirming}
                onClick={() => confirmSavedCode()}
              >
                {confirming ? "Checking…" : "Done"}
              </Button>
            </div>
          </>
        ) : loading ? (
          <p role="status" className="text-sm text-muted-foreground">
            Loading recovery status...
          </p>
        ) : !status ? (
          <div className="space-y-3">
            <p role="status" className="text-sm text-muted-foreground">
              Recovery status is unavailable. Refresh before making changes.
            </p>
            <Button variant="outline" onClick={() => void refresh()}>
              Refresh recovery status
            </Button>
          </div>
        ) : (
          <>
            <p className="text-sm font-medium">
              {status.enrolled ? "Recovery is enabled" : "Recovery is not enabled"}
            </p>
            {status.enrolled && status.last_used_at && (
              <p className="text-sm text-muted-foreground">
                Last used: {new Date(status.last_used_at).toLocaleString()} · Used{" "}
                {status.use_count} time{status.use_count === 1 ? "" : "s"}
              </p>
            )}
            {!status.enrolled && (
              <p className="text-sm text-muted-foreground">
                Without a recovery code, resetting your password deletes your encrypted data.
              </p>
            )}
            {action ? (
              <form onSubmit={submit} className="space-y-4">
                <p className="text-sm text-muted-foreground">
                  {action === "replace"
                    ? "Your current code will stop working immediately. Save the replacement before leaving this page."
                    : action === "disable"
                      ? "Your code will stop working. Future password resets will delete your encrypted data unless you enable recovery again."
                      : "Confirm your current password to create a recovery code."}
                </p>
                <div className="grid gap-2">
                  <Label htmlFor="recovery-password">Current password</Label>
                  <Input
                    id="recovery-password"
                    type="password"
                    autoComplete="current-password"
                    value={password}
                    disabled={busy}
                    required
                    onChange={(event) => setPassword(event.target.value)}
                  />
                </div>
                <div className="flex flex-wrap justify-end gap-2">
                  <Button
                    type="button"
                    variant="outline"
                    disabled={busy}
                    onClick={() => {
                      setAction(null);
                      setPassword("");
                      setError(null);
                    }}
                  >
                    Cancel
                  </Button>
                  <Button
                    type="submit"
                    variant={action === "disable" ? "destructive" : "default"}
                    disabled={busy || !password}
                  >
                    {busy
                      ? "Updating recovery..."
                      : action === "enable"
                        ? "Create recovery code"
                        : action === "replace"
                          ? "Replace recovery code"
                          : "Disable recovery code"}
                  </Button>
                </div>
              </form>
            ) : (
              <div className="flex flex-wrap gap-2">
                <Button
                  variant={status.enrolled ? "outline" : "default"}
                  onClick={() => {
                    setAction(status.enrolled ? "replace" : "enable");
                    setNotice(null);
                    setError(null);
                  }}
                >
                  {status.enrolled ? "Replace recovery code" : "Enable recovery"}
                </Button>
                {status.enrolled && (
                  <Button
                    variant="outline"
                    onClick={() => {
                      setAction("disable");
                      setNotice(null);
                      setError(null);
                    }}
                  >
                    Disable recovery
                  </Button>
                )}
              </div>
            )}
          </>
        )}
      </div>
    </SettingsSection>
  );
}
