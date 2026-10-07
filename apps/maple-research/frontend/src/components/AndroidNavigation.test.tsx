import { afterEach, describe, expect, spyOn, test } from "bun:test";
import { Children, isValidElement, type ReactNode } from "react";
import * as platform from "@/utils/platform";
import { Route as PrivacyRoute } from "@/routes/privacy";
import { Route as TermsRoute } from "@/routes/terms";
import { Footer } from "./Footer";

function destinations(node: ReactNode): string[] {
  return Children.toArray(node).flatMap((child) => {
    if (!isValidElement<{ href?: string; to?: string; children?: ReactNode }>(child)) return [];
    const destination = child.props.href ?? child.props.to;
    return [...(destination ? [destination] : []), ...destinations(child.props.children)];
  });
}

describe("Android information navigation", () => {
  const cleanup: Array<() => void> = [];
  function setAndroid(value: boolean) {
    const androidSpy = spyOn(platform, "isAndroid").mockReturnValue(value);
    const tauriSpy = spyOn(platform, "isTauri").mockReturnValue(value);
    cleanup.push(
      () => androidSpy.mockRestore(),
      () => tauriSpy.mockRestore()
    );
  }
  afterEach(() => {
    cleanup
      .splice(0)
      .reverse()
      .forEach((restore) => restore());
  });

  test("footer retains local legal pages and email support without marketing exits", () => {
    setAndroid(true);
    expect(destinations(Footer())).toEqual(["/privacy", "/terms", "mailto:support@trymaple.ai"]);
  });

  test("web footer preserves marketing and community navigation", () => {
    setAndroid(false);
    const links = destinations(Footer());
    expect(links).toContain("/pricing");
    expect(links).toContain("https://blog.trymaple.ai");
    expect(links).toContain("https://discord.gg/ch2gjZAMGy");
  });

  for (const [name, route] of [
    ["Privacy", PrivacyRoute],
    ["Terms", TermsRoute]
  ] as const) {
    test(`${name} keeps the website reference unlinked on Android`, () => {
      const component = route.options.component as () => ReactNode;
      setAndroid(true);
      expect(destinations(component())).not.toContain("https://trymaple.ai");
    });
    test(`${name} preserves the website link outside Android`, () => {
      const component = route.options.component as () => ReactNode;
      setAndroid(false);
      expect(destinations(component())).toContain("https://trymaple.ai");
    });
  }
});
