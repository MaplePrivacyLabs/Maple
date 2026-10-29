import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import { AgentModeMovedNotice } from "./AgentModeMovedPage";

describe("AgentModeMovedNotice", () => {
  test("links to the Maple Work download and the last Agent Mode release", () => {
    const markup = renderToStaticMarkup(<AgentModeMovedNotice />);

    expect(markup).toContain("Agent Mode is now Maple Work");
    expect(markup).toContain('href="https://maple.ai/work/download"');
    expect(markup).toContain(
      'href="https://github.com/MaplePrivacyLabs/Maple/releases/tag/v3.4.5"'
    );
  });
});
