import { describe, expect, it } from "bun:test";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";

import { MarkdownContent, ThinkingBlock } from "./markdown";

function renderMarkdown(content: string): string {
  return renderToStaticMarkup(React.createElement(MarkdownContent, { content }));
}

describe("MarkdownContent images", () => {
  it("renders image alt text without loading remote or local URL schemes", () => {
    const rendered = renderMarkdown(
      [
        "![remote](https://example.com/tracker.png)",
        "![relative](/local-image.png)",
        "![data](data:image/png;base64,abc)",
        "![blob](blob:https://trymaple.ai/image-id)"
      ].join("\n\n")
    );

    expect(rendered).not.toContain("<img");
    expect(rendered).not.toContain("tracker.png");
    expect(rendered).not.toContain("local-image.png");
    expect(rendered).not.toContain("data:image");
    expect(rendered).not.toContain("blob:");
    expect(rendered).toContain("remote");
    expect(rendered).toContain("relative");
    expect(rendered).toContain("data");
    expect(rendered).toContain("blob");
  });

  it("continues to render ordinary links", () => {
    const rendered = renderMarkdown("[Maple](https://trymaple.ai)");

    expect(rendered).toContain('href="https://trymaple.ai"');
    expect(rendered).toContain(">Maple</a>");
  });

  it("omits images without alt text", () => {
    const rendered = renderMarkdown("![](https://example.com/hidden.png)");

    expect(rendered).not.toContain("<img");
    expect(rendered).not.toContain("hidden.png");
    expect(rendered).not.toContain("<span");
  });
});

describe("ThinkingBlock", () => {
  it("renders an untimed shimmer while a main-chat thought is streaming", () => {
    const renderStreaming = (duration: number) =>
      renderToStaticMarkup(
        React.createElement(ThinkingBlock, {
          content: "Streaming reasoning tokens.",
          isThinking: true,
          duration
        })
      );
    const beforeSwitch = renderStreaming(20);
    const afterSwitch = renderStreaming(1);

    expect(afterSwitch).toBe(beforeSwitch);
    expect(afterSwitch).toContain(">Thinking<span");
    expect(afterSwitch).toContain("thinking-shimmer");
    expect(afterSwitch).toContain("thinking-shimmer-highlight");
    expect(afterSwitch).toContain('aria-hidden="true"');
    expect(afterSwitch).not.toContain("Thinking for");
    expect(afterSwitch).not.toContain("seconds");
  });

  it("keeps the completed main-chat thought duration", () => {
    const completed = renderToStaticMarkup(
      React.createElement(ThinkingBlock, {
        content: "Finished reasoning tokens.",
        isThinking: false,
        duration: 23
      })
    );

    expect(completed).toContain("Thought for 23 seconds");
    expect(completed).not.toContain("thinking-shimmer");
  });

  it("keeps estimating a completed main-chat thought when no duration is reported", () => {
    const completed = renderToStaticMarkup(
      React.createElement(ThinkingBlock, {
        content: Array.from({ length: 52 }, () => "word").join(" "),
        isThinking: false
      })
    );

    expect(completed).toContain("Thought for 2 seconds");
    expect(completed).not.toContain("thinking-shimmer");
  });
});

describe("MarkdownContent math", () => {
  it("renders inline and display math through the patched KaTeX renderer", () => {
    const rendered = renderMarkdown("Inline $x^2$\n\n$$\n\\frac{1}{2}\n$$");

    expect(rendered).toContain('class="katex"');
    expect(rendered).toContain('class="katex-display"');
    expect(rendered).toContain("<math");
  });

  it("does not inherit trusted rendering from a polluted Object prototype", () => {
    const previous = Object.getOwnPropertyDescriptor(Object.prototype, "trust");
    Object.defineProperty(Object.prototype, "trust", {
      value: true,
      configurable: true
    });
    try {
      const rendered = renderMarkdown("$\\href{https://example.com/untrusted}{unsafe}$");

      expect(rendered).not.toContain('href="https://example.com/untrusted"');
    } finally {
      if (previous) {
        Object.defineProperty(Object.prototype, "trust", previous);
      } else {
        Reflect.deleteProperty(Object.prototype, "trust");
      }
    }
  });
});
