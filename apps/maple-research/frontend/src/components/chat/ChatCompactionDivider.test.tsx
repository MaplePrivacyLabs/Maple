import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import { ChatCompactionDivider } from "./ChatCompactionDivider";

describe("conversation compaction divider", () => {
  test("quietly announces actual compaction progress as a standalone line", () => {
    const markup = renderToStaticMarkup(<ChatCompactionDivider status="compacting" />);
    expect(markup).toContain('role="status"');
    expect(markup).toContain('aria-live="polite"');
    expect(markup).toContain(">compacting</span>");
    expect(markup).toContain("italic");
    expect(markup).toContain("text-muted-foreground");
    expect(markup).not.toContain("animate-");
    expect(markup).not.toContain("m-avatar");
    expect(markup).not.toContain("button");
  });

  test("retains a completed history anchor without repeatedly announcing old checkpoints", () => {
    const markup = renderToStaticMarkup(
      <ChatCompactionDivider status="completed" historyAnchorId="checkpoint-id" />
    );
    expect(markup).toContain(">compacted</span>");
    expect(markup).toContain('data-history-anchor-ids="checkpoint-id"');
    expect(markup).not.toContain('role="status"');
    expect(markup).not.toContain("aria-live");
  });
});
