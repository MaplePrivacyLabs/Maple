import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import { SidebarToggle } from "./Sidebar";

describe("SidebarToggle", () => {
  test("renders the open-sidebar button", () => {
    const markup = renderToStaticMarkup(<SidebarToggle onToggle={() => {}} />);

    expect(markup).toContain('aria-label="Open sidebar"');
    expect(markup).toContain("lucide-menu");
  });
});
