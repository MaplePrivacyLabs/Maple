import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import { ToolActivityCard } from "./ToolActivityCard";

describe("ToolActivityCard", () => {
  test("renders the compact visual hierarchy with an accessible kind label", () => {
    const markup = renderToStaticMarkup(
      <ToolActivityCard kind="web" title="Web Search: maple" status="completed" />
    );

    expect(markup).toContain('aria-label="Web tool: Web Search: maple, Completed"');
    expect(markup).toContain('title="Web Search: maple"');
    expect(markup).toContain("text-[13px]");
    expect(markup).toContain("Completed");
    expect(markup).toContain("text-maple-success");
    expect(markup).not.toContain("<details");
  });

  test("supports custom status copy while announcing status changes", () => {
    const markup = renderToStaticMarkup(
      <ToolActivityCard kind="shell" title="Terminal: pwd" status="active" statusLabel="Running" />
    );

    expect(markup).toContain('role="status"');
    expect(markup).toContain('aria-live="polite"');
    expect(markup).toContain('aria-label="Shell command: Terminal: pwd, Running"');
    expect(markup).toContain("Running");
  });

  test("uses a disclosure for details and opens failed tools", () => {
    const markup = renderToStaticMarkup(
      <ToolActivityCard kind="generic" title="lookup_record" status="error">
        <pre>Failure details</pre>
      </ToolActivityCard>
    );

    expect(markup).toContain("<details open");
    expect(markup).toContain("border-destructive/35");
    expect(markup).toContain("Failed");
    expect(markup).toContain("Failure details");
  });

  test("keeps incomplete details closed and distinct from failed and completed", () => {
    const markup = renderToStaticMarkup(
      <ToolActivityCard kind="web" title="Web Search" status="incomplete">
        <pre>Partial result</pre>
      </ToolActivityCard>
    );

    expect(markup).toContain("<details");
    expect(markup).not.toContain("<details open");
    expect(markup).toContain("Incomplete");
    expect(markup).toContain("text-[hsl(var(--maple-warning-foreground))]");
    expect(markup).not.toContain("bg-destructive/5");
  });
});
