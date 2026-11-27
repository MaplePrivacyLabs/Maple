import {
  classifyChatLimitFailure,
  classifyChatStreamLimitFailure,
  isChatPlanAccessDeniedError,
  type ChatLimitFailure
} from "./chatResponseErrors";

type ChatLimitDialog = "context" | "tokens" | "usage";

type ChatLimitDialogOptions = {
  isRunCurrent: () => boolean;
  isRuntimeSelected: () => boolean;
  showUpgradeDialog: (feature: "usage" | "tokens") => void;
  showContextLimitDialog: () => void;
};

type ChatAccessErrorOptions = ChatLimitDialogOptions & {
  error: unknown;
  productName?: string | null;
  restoreTurn: (message: string) => boolean;
};

type ChatStreamLimitOptions = ChatLimitDialogOptions & {
  event: unknown;
  productName?: string | null;
};

/** The message and dialog for a usage, Free-token or context limit. */
export function chatLimitPresentation(
  failure: ChatLimitFailure,
  productName?: string | null
): { message: string; dialog: ChatLimitDialog } {
  if (failure.kind === "context") {
    return { message: "Your message exceeds the context limit for this model.", dialog: "context" };
  }
  if (failure.kind === "freeToken") {
    return {
      message:
        "This conversation is too long for the free tier. Upgrade to Pro for longer conversations.",
      dialog: "tokens"
    };
  }
  const plan = productName?.toLowerCase();
  const isFreeTier = !plan || plan === "free";
  const isPro = plan?.includes("pro") && !plan.includes("max");
  return {
    message: isFreeTier
      ? "You've reached your daily usage limit. Upgrade to Pro for more chats."
      : isPro
        ? "You've reached your monthly Pro limit. Upgrade to Max for 10x more usage."
        : "You've reached your monthly usage limit. Please wait for the next billing cycle.",
    dialog: "usage"
  };
}

/** Only the run that owns the selected chat may open its dialog. */
function showLimitDialog(dialog: ChatLimitDialog, options: ChatLimitDialogOptions) {
  if (!options.isRunCurrent() || !options.isRuntimeSelected()) return;
  if (dialog === "context") options.showContextLimitDialog();
  else options.showUpgradeDialog(dialog);
}

/** Restore a rejected turn before publishing a dialog owned by its selected run. */
export function handleChatAccessError({
  error,
  productName,
  restoreTurn,
  ...dialogs
}: ChatAccessErrorOptions): boolean {
  const failure = classifyChatLimitFailure(error);
  if (!failure) {
    if (!isChatPlanAccessDeniedError(error)) return false;
    restoreTurn("This model or feature is not available on your current plan.");
    return true;
  }

  const { message, dialog } = chatLimitPresentation(failure, productName);
  if (restoreTurn(message)) showLimitDialog(dialog, dialogs);
  return true;
}

/**
 * Publish the dialog for a limit that ended an accepted Responses turn. The
 * server already saved that turn, so nothing is restored to the composer.
 * Returns the message to show, or null for any other stream failure.
 */
export function handleChatStreamLimitFailure({
  event,
  productName,
  ...dialogs
}: ChatStreamLimitOptions): string | null {
  const failure = classifyChatStreamLimitFailure(event);
  if (!failure) return null;
  const { message, dialog } = chatLimitPresentation(failure, productName);
  showLimitDialog(dialog, dialogs);
  return message;
}
