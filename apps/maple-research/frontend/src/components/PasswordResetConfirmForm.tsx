import { useState } from "react";
import { useBlocker, useNavigate } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import { PasswordResetFlow } from "./PasswordResetFlow";

interface PasswordResetConfirmFormProps {
  email: string;
  secret: string;
  onRestart?: () => void;
}

export function PasswordResetConfirmForm({
  email,
  secret,
  onRestart
}: PasswordResetConfirmFormProps) {
  const os = useOpenSecret();
  const navigate = useNavigate();
  const [busy, setBusy] = useState(false);
  useBlocker({ shouldBlockFn: () => busy, disabled: !busy, enableBeforeUnload: busy });
  return (
    <PasswordResetFlow
      email={email}
      secret={secret}
      api={os}
      onBusyChange={setBusy}
      onComplete={() => {
        void navigate({ to: "/", replace: true });
      }}
      onRestart={
        onRestart ??
        (() => {
          void navigate({ to: "/password-reset", replace: true });
        })
      }
    />
  );
}
