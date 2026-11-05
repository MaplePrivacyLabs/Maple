import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { RecoveryCodeResponse, RecoveryCodeStatus } from "@mapleai/sdk";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { SettingsSection } from "./SettingsPage";

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
  onLockChange
}: {
  api: RecoveryApi;
  onLockChange: (locked: boolean) => void;
}) {
  const [status, setStatus] = useState<RecoveryCodeStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [action, setAction] = useState<Action | null>(null);
  const [password, setPassword] = useState("");
  const [code, setCode] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
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
        setStatus({ enrolled: false, enrolled_at: null, last_used_at: null, use_count: 0 });
        setNotice("Recovery code disabled. Your previous code no longer works.");
      } else {
        const result = await (action === "enable"
          ? api.enrollRecoveryCode(password)
          : api.rotateRecoveryCode(password));
        if (!alive.current) return;
        ++statusRevision.current;
        setCode(result.recovery_code);
        setSaved(false);
        setStatus({ enrolled: true, enrolled_at: null, last_used_at: null, use_count: 0 });
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
        setError("Couldn't copy the code. Select it and copy it manually, or save a text file.");
    }
  }

  function downloadCode() {
    if (!code) return;
    let url: string | undefined;
    try {
      url = URL.createObjectURL(
        new Blob(
          [
            `Maple recovery code\n\n${code}\n\nKeep this code private. Replacing or disabling it makes this copy invalid.\n`
          ],
          { type: "text/plain" }
        )
      );
      const link = document.createElement("a");
      link.href = url;
      link.download = "maple-recovery-code.txt";
      link.click();
    } catch {
      setError("Couldn't save a file. Copy the code and store it somewhere safe instead.");
    } finally {
      if (url) URL.revokeObjectURL(url);
    }
  }

  return (
    <SettingsSection
      title={code ? "Save your recovery code" : "Recovery code"}
      description="Reset your password without losing your encrypted data. Keep your recovery code somewhere safe, such as a password manager."
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
              <Button variant="outline" onClick={downloadCode}>
                Save text file
              </Button>
            </div>
            <label className="flex items-start gap-2 text-sm">
              <input
                type="checkbox"
                checked={saved}
                onChange={(event) => setSaved(event.target.checked)}
                className="mt-0.5"
              />
              I've saved this recovery code somewhere safe.
            </label>
            <div className="flex justify-end">
              <Button
                disabled={!saved}
                onClick={() => {
                  setCode(null);
                  setSaved(false);
                  setNotice("Recovery is enabled. Keep your saved code private.");
                  void refresh();
                }}
              >
                Done
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
