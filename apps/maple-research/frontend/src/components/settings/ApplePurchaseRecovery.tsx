import { useEffect, useRef, useState } from "react";
import { Loader2, RefreshCw } from "lucide-react";
import { appleBillingErrorMessage } from "@/billing/appleBillingLifecycle";
import type { AppleBillingContextValue } from "@/billing/useAppleBilling";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { SettingsSection } from "./SettingsPage";

export function ApplePurchaseRecovery({ apple }: { apple: AppleBillingContextValue }) {
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  const lifetime = useRef({ active: true, version: 0 });
  useEffect(() => {
    const current = lifetime.current;
    current.active = true;
    return () => {
      current.active = false;
      current.version++;
    };
  }, []);
  if (!apple.available) return null;

  const run = async (restore: boolean) => {
    if (!lifetime.current.active || running || apple.busy || !apple.ready) return;
    const version = ++lifetime.current.version;
    const current = () => lifetime.current.active && lifetime.current.version === version;
    setRunning(true);
    setMessage(null);
    setError(null);
    try {
      if (restore) await apple.restore();
      else await apple.retry();
      if (!current()) return;
      setMessage("Apple purchases checked. Your current plan is shown above.");
    } catch (failure) {
      if (current()) setError(appleBillingErrorMessage(failure));
    } finally {
      if (current()) setRunning(false);
    }
  };
  const failure = error ?? apple.recoveryError;
  const busy = running || apple.busy;

  return (
    <SettingsSection
      title="Apple purchases"
      description="Restore purchases made with your Apple Account. Apple may ask you to sign in."
    >
      <div className="flex flex-wrap gap-2">
        <Button variant="outline" onClick={() => void run(true)} disabled={busy || !apple.ready}>
          {busy ? (
            <Loader2 className="mr-2 h-4 w-4 animate-spin" />
          ) : (
            <RefreshCw className="mr-2 h-4 w-4" />
          )}
          Restore Apple purchases
        </Button>
        {failure && (
          <Button variant="outline" onClick={() => void run(false)} disabled={busy || !apple.ready}>
            Retry confirmation
          </Button>
        )}
      </div>
      {failure && (
        <Alert variant="destructive" className="mt-4">
          <AlertDescription>{failure}</AlertDescription>
        </Alert>
      )}
      {message && (
        <p role="status" className="mt-3 text-sm text-muted-foreground">
          {message}
        </p>
      )}
    </SettingsSection>
  );
}
