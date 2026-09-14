import { ArrowUp, FilePenLine, Trash } from "lucide-react";
import { Button } from "@/components/ui/button";
import type { QueuedComposerMessage } from "@/services/composerQueue";
import { cn } from "@/utils/utils";

export type { QueuedComposerMessage } from "@/services/composerQueue";

export const QUEUED_MESSAGE_EDIT_PLACEHOLDER =
  "Edit the queued message, then send to keep its place...";

export function QueuedComposerMessages<T extends QueuedComposerMessage>({
  items,
  className,
  editingQueueId = null,
  getFallbackLabel,
  onRemove,
  onEdit,
  onSendNow,
  sendNowDisabled = false
}: {
  items: readonly T[];
  className?: string;
  editingQueueId?: string | null;
  getFallbackLabel?: (item: T) => string;
  onRemove?: (queueId: string) => void;
  onEdit?: (queueId: string) => void;
  onSendNow?: (queueId: string) => void;
  sendNowDisabled?: boolean;
}) {
  if (items.length === 0) return null;

  return (
    <div className={cn("flex flex-col gap-1 px-3 pt-2", className)}>
      {items.map((item) => (
        <div
          key={item.queueId}
          className={cn(
            "flex items-center gap-1 rounded-lg bg-muted/70 px-2 py-1 text-left text-xs text-muted-foreground",
            item.queueId === editingQueueId && "bg-muted text-foreground ring-1 ring-border"
          )}
        >
          <span className="min-w-0 flex-1 truncate" title={item.text}>
            {item.text || getFallbackLabel?.(item) || "Queued message"}
          </span>
          {onRemove ? (
            <button
              type="button"
              className="rounded-md p-1 text-muted-foreground transition-colors hover:bg-background hover:text-foreground"
              onClick={() => onRemove(item.queueId)}
              aria-label="Remove queued message"
            >
              <Trash className="h-3.5 w-3.5" />
            </button>
          ) : null}
          {onEdit ? (
            <button
              type="button"
              className="rounded-md p-1 text-muted-foreground transition-colors hover:bg-background hover:text-foreground"
              onClick={() => onEdit(item.queueId)}
              aria-label="Edit queued message"
            >
              <FilePenLine className="h-3.5 w-3.5" />
            </button>
          ) : null}
          {onSendNow ? (
            <button
              type="button"
              className="rounded-md p-1 text-muted-foreground transition-colors hover:bg-background hover:text-foreground disabled:pointer-events-none disabled:opacity-40"
              onClick={() => onSendNow(item.queueId)}
              disabled={sendNowDisabled}
              title="Send into the current turn"
              aria-label="Send queued message into the current turn"
            >
              <ArrowUp className="h-3.5 w-3.5" />
            </button>
          ) : null}
        </div>
      ))}
    </div>
  );
}

export function DiscardQueuedMessageEditButton({ onDiscard }: { onDiscard: () => void }) {
  return (
    <Button
      type="button"
      size="sm"
      variant="ghost"
      className="h-8 px-2 text-xs text-muted-foreground"
      onClick={onDiscard}
    >
      Discard
    </Button>
  );
}
