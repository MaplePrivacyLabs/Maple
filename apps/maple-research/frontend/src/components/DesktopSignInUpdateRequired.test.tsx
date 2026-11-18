import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import { DesktopSignInUpdateRequired } from "./DesktopSignInUpdateRequired";

describe("DesktopSignInUpdateRequired", () => {
  test("tells an unsupported release to update and links to the download page", () => {
    const markup = renderToStaticMarkup(<DesktopSignInUpdateRequired />);

    expect(markup).toContain("Update Maple to sign in");
    expect(markup).toContain("no longer supported");
    expect(markup).toContain('href="https://maple.ai/research#download"');
  });
});
