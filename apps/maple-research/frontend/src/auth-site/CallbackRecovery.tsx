import { useState } from "react";
import { Button } from "@/components/ui/button";

export function ApplePopupRecovery() {
  return (
    <p role="alert">
      Apple sign-in must finish in its popup. Return to the Maple sign-in tab and try again,
      allowing popups when your browser asks.
    </p>
  );
}

export function CallbackRecovery() {
  const [copyStatus, setCopyStatus] = useState<string | null>(null);

  const copyAddress = async () => {
    try {
      // Keep this user initiated: the address contains a one-time authorization code.
      await navigator.clipboard.writeText(window.location.href);
      setCopyStatus("Address copied. Paste it only into the Maple sign-in you started.");
    } catch {
      setCopyStatus("Copy the full address from your browser's address bar instead.");
    }
  };

  return (
    <div className="space-y-3 text-sm text-muted-foreground">
      <p>
        If Maple Agent asked you to paste a callback URL, copy this page's full address and paste it
        into that sign-in window. Otherwise, start a new sign-in in Maple.
      </p>
      <p>Only paste this address into the Maple sign-in you started. Do not share it.</p>
      <Button type="button" variant="outline" onClick={copyAddress}>
        Copy callback address
      </Button>
      {copyStatus && <p role="status">{copyStatus}</p>}
    </div>
  );
}
