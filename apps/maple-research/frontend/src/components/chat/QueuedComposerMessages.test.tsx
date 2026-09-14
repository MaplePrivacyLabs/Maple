import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import {
  DiscardQueuedMessageEditButton,
  QUEUED_MESSAGE_EDIT_PLACEHOLDER,
  QueuedComposerMessages
} from "./QueuedComposerMessages";

describe("queued composer messages", () => {
  test("renders the edit state and Agent action order", () => {
    const markup = renderToStaticMarkup(
      <QueuedComposerMessages
        items={[{ queueId: "q1", text: "queued" }]}
        editingQueueId="q1"
        onRemove={() => {}}
        onEdit={() => {}}
        onSendNow={() => {}}
      />
    );

    expect(markup).toContain("bg-muted text-foreground ring-1 ring-border");
    expect(markup.indexOf('aria-label="Remove queued message"')).toBeLessThan(
      markup.indexOf('aria-label="Edit queued message"')
    );
    expect(markup.indexOf('aria-label="Edit queued message"')).toBeLessThan(
      markup.indexOf('aria-label="Send queued message into the current turn"')
    );
  });

  test("supports an attachment fallback without adding a send-now action", () => {
    const markup = renderToStaticMarkup(
      <QueuedComposerMessages
        items={[{ queueId: "q1", text: "", attachmentCount: 2 }]}
        getFallbackLabel={(item) => `${item.attachmentCount} attachment(s)`}
        onRemove={() => {}}
        onEdit={() => {}}
      />
    );

    expect(markup).toContain("2 attachment(s)");
    expect(markup).not.toContain("Send queued message into the current turn");
  });

  test("disables only the optional send-now action", () => {
    const markup = renderToStaticMarkup(
      <QueuedComposerMessages
        items={[{ queueId: "q1", text: "queued" }]}
        onRemove={() => {}}
        onEdit={() => {}}
        onSendNow={() => {}}
        sendNowDisabled
      />
    );

    expect(markup.match(/disabled=""/g)).toHaveLength(1);
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
