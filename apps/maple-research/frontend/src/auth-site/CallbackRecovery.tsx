export function ApplePopupRecovery() {
  return (
    <p role="alert">
      Apple sign-in must finish in its popup. Return to the Maple sign-in tab and try again,
      allowing popups when your browser asks.
    </p>
  );
}

export function CallbackRecovery() {
  return (
    <p className="text-sm text-muted-foreground">
      Return to Maple and start a new sign-in. You can close this page.
    </p>
  );
}
