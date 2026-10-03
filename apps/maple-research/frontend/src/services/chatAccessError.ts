import { classifyChatLimitFailure, isChatPlanAccessDeniedError } from "./chatResponseErrors";

type ChatAccessErrorOptions = {
  error: unknown;
  productName?: string | null;
  restoreTurn: (message: string) => boolean;
  isRunCurrent: () => boolean;
  isRuntimeSelected: () => boolean;
  showUpgradeDialog: (feature: "usage" | "tokens") => void;
  showContextLimitDialog: () => void;
};

/** Restore a rejected turn before publishing a dialog owned by its selected run. */
export function handleChatAccessError({
  error,
  productName,
  restoreTurn,
  isRunCurrent,
  isRuntimeSelected,
  showUpgradeDialog,
  showContextLimitDialog
}: ChatAccessErrorOptions): boolean {
  const failure = classifyChatLimitFailure(error);
  if (!failure) {
    if (!isChatPlanAccessDeniedError(error)) return false;
    restoreTurn("This model or feature is not available on your current plan.");
    return true;
  }

  let message: string;
  let dialog: "context" | "tokens" | "usage";
  if (failure.kind === "context") {
    message = "Your message exceeds the context limit for this model.";
    dialog = "context";
  } else if (failure.kind === "freeToken") {
    message =
      "This conversation is too long for the free tier. Upgrade to Pro for longer conversations.";
    dialog = "tokens";
  } else {
    const plan = productName?.toLowerCase();
    const isFreeTier = !plan || plan === "free";
    const isPro = plan?.includes("pro") && !plan.includes("max");
    message = isFreeTier
      ? "You've reached your daily usage limit. Upgrade to Pro for more chats."
      : isPro
        ? "You've reached your monthly Pro limit. Upgrade to Max for 10x more usage."
        : "You've reached your monthly usage limit. Please wait for the next billing cycle.";
    dialog = "usage";
  }

  if (restoreTurn(message) && isRunCurrent() && isRuntimeSelected()) {
    if (dialog === "context") showContextLimitDialog();
    else showUpgradeDialog(dialog);
  }
  return true;
}
