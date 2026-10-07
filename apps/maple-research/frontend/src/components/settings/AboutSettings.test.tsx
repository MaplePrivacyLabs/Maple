import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import * as router from "@tanstack/react-router";
import * as platform from "@/utils/platform";
import { AboutSettings, resolveAboutSettingsGating } from "./AboutSettings";

function textContent(node: ReactTestInstance): string {
  return node.children
    .map((child) => (typeof child === "string" ? child : textContent(child)))
    .join("");
}

describe("resolveAboutSettingsGating", () => {
  test("shows downloads only in the web view", () => {
    expect(
      resolveAboutSettingsGating({
        isTauriDesktop: () => false,
        isWeb: () => true
      })
    ).toEqual({ supportsDesktopUpdates: false, showAppDownloads: true });
  });

  test("shows desktop updates only on Tauri desktop", () => {
    expect(
      resolveAboutSettingsGating({
        isTauriDesktop: () => true,
        isWeb: () => false
      })
    ).toEqual({ supportsDesktopUpdates: true, showAppDownloads: false });
  });

  test("hides both sections on Tauri mobile", () => {
    expect(
      resolveAboutSettingsGating({
        isTauriDesktop: () => false,
        isWeb: () => false
      })
    ).toEqual({ supportsDesktopUpdates: false, showAppDownloads: false });
  });
});

describe("AboutSettings", () => {
  let renderer: ReactTestRenderer | null = null;
  let restoreLink: (() => void) | undefined;

  beforeEach(() => {
    const linkSpy = spyOn(router, "Link").mockImplementation((({
      children,
      to
    }: {
      children: React.ReactNode;
      to: string;
    }) => <a href={to}>{children}</a>) as unknown as typeof router.Link);
    restoreLink = () => linkSpy.mockRestore();
  });

  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    renderer = null;
    restoreLink?.();
    restoreLink = undefined;
  });

  test("uses the live platform defaults in this web test environment", async () => {
    await act(async () => {
      renderer = create(<AboutSettings />);
      await Promise.resolve();
    });

    expect(textContent(renderer!.root)).toContain("Get the Maple app");
    expect(textContent(renderer!.root)).not.toContain("Automatic updates");
  });

  test("shows app downloads in the web view and hides desktop update settings", async () => {
    await act(async () => {
      renderer = create(<AboutSettings supportsDesktopUpdates={false} showAppDownloads />);
      await Promise.resolve();
    });

    expect(textContent(renderer!.root)).toContain("Get the Maple app");
    expect(textContent(renderer!.root)).not.toContain("Automatic updates");
  });

  test("hides both native-only sections on Tauri mobile", () => {
    act(() => {
      renderer = create(<AboutSettings supportsDesktopUpdates={false} showAppDownloads={false} />);
    });

    expect(textContent(renderer!.root)).not.toContain("Automatic updates");
    expect(textContent(renderer!.root)).not.toContain("Get the Maple app");
  });

  test("Android keeps legal pages inside the app and retains email support", () => {
    const androidSpy = spyOn(platform, "isAndroid").mockReturnValue(true);
    try {
      act(() => {
        renderer = create(
          <AboutSettings supportsDesktopUpdates={false} showAppDownloads={false} />
        );
      });
      const links = renderer!.root.findAllByType("a").map((link) => link.props.href);
      expect(links).toContain("/privacy");
      expect(links).toContain("/terms");
      expect(links.some((href) => typeof href === "string" && href.startsWith("https://"))).toBe(
        false
      );
      expect(textContent(renderer!.root)).toContain("Contact us");
    } finally {
      androidSpy.mockRestore();
    }
  });
});
