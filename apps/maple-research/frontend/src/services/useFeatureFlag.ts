import { useEffect, useState } from "react";
import { flagsClient } from "./flags";

/**
 * One user's flag value: `undefined` while it loads, `false` when the flag
 * service is unreachable (optional features stay hidden). Seeds from the
 * user-scoped cache, then revalidates.
 */
export function useFeatureFlag(userId: string | undefined, key: string): boolean | undefined {
  const [loaded, setLoaded] = useState<{ userId: string; key: string; enabled: boolean } | null>(
    null
  );

  useEffect(() => {
    if (!userId) return;
    let disposed = false;
    void flagsClient.isEnabled(userId, key).then(
      (enabled) => {
        if (!disposed) setLoaded({ userId, key, enabled });
      },
      (error: unknown) => {
        console.warn("Unable to load optional feature flags; keeping them hidden.", error);
        if (!disposed) setLoaded({ userId, key, enabled: false });
      }
    );
    return () => {
      disposed = true;
    };
  }, [userId, key]);

  if (!userId) return undefined;
  if (loaded?.userId === userId && loaded.key === key) return loaded.enabled;
  return flagsClient.peekIsEnabled(userId, key);
}
