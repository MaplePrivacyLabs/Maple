import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import {
  DiscardQueuedMessageEditButton,
  QUEUED_MESSAGE_EDIT_PLACEHOLDER,
  QueuedComposerMessages
} from "./QueuedComposerMessages";

describe("queued composer messages", () => {
  test("renders the edit state and action order", () => {
    const markup = renderToStaticMarkup(
      <QueuedComposerMessages
        items={[{ queueId: "q1", text: "queued" }]}
        editingQueueId="q1"
        onRemove={() => {}}
        onEdit={() => {}}
      />
    );

    expect(markup).toContain("bg-muted text-foreground ring-1 ring-border");
    expect(markup.indexOf('aria-label="Remove queued message"')).toBeLessThan(
      markup.indexOf('aria-label="Edit queued message"')
    );
  });

  test("supports an attachment fallback", () => {
    const markup = renderToStaticMarkup(
      <QueuedComposerMessages
        items={[{ queueId: "q1", text: "", attachmentCount: 2 }]}
        getFallbackLabel={(item) => `${item.attachmentCount} attachment(s)`}
        onRemove={() => {}}
        onEdit={() => {}}
      />
    );

    expect(markup).toContain("2 attachment(s)");
  });

  test("accepts layout clearance for an overlaid composer control", () => {
    const markup = renderToStaticMarkup(
      <QueuedComposerMessages items={[{ queueId: "q1", text: "queued" }]} className="pr-10" />
    );

    expect(markup).toContain("pr-10");
  });

  test("shares the edit prompt and discard control", () => {
    const markup = renderToStaticMarkup(<DiscardQueuedMessageEditButton onDiscard={() => {}} />);

    expect(QUEUED_MESSAGE_EDIT_PLACEHOLDER).toBe(
      "Edit the queued message, then send to keep its place..."
    );
    expect(markup).toContain(">Discard</button>");
  });

  test("renders nothing for an empty queue", () => {
    expect(renderToStaticMarkup(<QueuedComposerMessages items={[]} />)).toBe("");
  });
});
