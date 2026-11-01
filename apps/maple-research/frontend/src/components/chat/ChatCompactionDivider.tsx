export function ChatCompactionDivider({
  status,
  historyAnchorId
}: {
  status: "compacting" | "completed";
  historyAnchorId?: string;
}) {
  const compacting = status === "compacting";
  return (
    <div
      className="my-4 flex items-center gap-3 px-2 text-xs text-muted-foreground"
      data-history-anchor-ids={historyAnchorId}
      data-compaction-status={status}
      role={compacting ? "status" : undefined}
      aria-live={compacting ? "polite" : undefined}
      aria-label={compacting ? "Compacting conversation context" : "Conversation context compacted"}
    >
      <span className="h-px flex-1 bg-border/60" aria-hidden="true" />
      <span className="shrink-0 italic">{compacting ? "compacting" : "compacted"}</span>
      <span className="h-px flex-1 bg-border/60" aria-hidden="true" />
    </div>
  );
}
