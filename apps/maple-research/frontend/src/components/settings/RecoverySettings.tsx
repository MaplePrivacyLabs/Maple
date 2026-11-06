import { useCallback, useState, type ComponentProps } from "react";
import { FEATURE_FLAGS } from "@/services/flags";
import { useFeatureFlag } from "@/services/useFeatureFlag";
import { RecoveryCodeSettings } from "./RecoveryCodeSettings";

/** Mount with an account-specific key, just like the one-time code it contains. */
export function RecoverySettings({
  userId,
  disabled,
  api,
  onLockChange
}: Pick<ComponentProps<typeof RecoveryCodeSettings>, "api" | "onLockChange"> & {
  userId: string;
  disabled: boolean;
}) {
  const enabled = useFeatureFlag(userId, FEATURE_FLAGS.RECOVERY);
  const [locked, setLocked] = useState(false);
  const handleLock = useCallback(
    (value: boolean) => {
      setLocked(value);
      onLockChange(value);
    },
    [onLockChange]
  );
  // A flag refresh must not discard a code that has already been issued.
  if (enabled !== true && !locked) return null;
  return (
    <fieldset disabled={disabled} className="min-w-0">
      <RecoveryCodeSettings api={api} onLockChange={handleLock} />
    </fieldset>
  );
}
