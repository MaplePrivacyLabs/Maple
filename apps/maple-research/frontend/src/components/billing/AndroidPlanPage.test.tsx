import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import { createElement, type ComponentType, type ReactNode } from "react";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import * as sdk from "@mapleai/sdk";
import * as router from "@tanstack/react-router";
import * as platform from "@/utils/platform";
import * as billing from "@/billing/billingService";
import { Route } from "@/routes/pricing";

describe("Android pricing route", () => {
  const cleanup: Array<() => void> = [];
  let renderer: ReactTestRenderer | undefined;

  beforeEach(async () => {
    await platform.waitForPlatform();
    const android = spyOn(platform, "isAndroid").mockReturnValue(true);
    const ios = spyOn(platform, "isIOS").mockReturnValue(false);
    const links = spyOn(router, "Link").mockImplementation((({
      children,
      to
    }: {
      children: ReactNode;
      to: string;
    }) => <a href={to}>{children}</a>) as typeof router.Link);
    cleanup.push(
      () => android.mockRestore(),
      () => ios.mockRestore(),
      () => links.mockRestore()
    );
  });
  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    renderer = undefined;
    cleanup
      .splice(0)
      .reverse()
      .forEach((restore) => restore());
  });

  for (const signedIn of [false, true]) {
    test(`${signedIn ? "signed-in" : "signed-out"} users get plan information without legacy checkout effects`, () => {
      const auth = spyOn(sdk, "useOpenSecret").mockReturnValue({
        auth: { user: signedIn ? { user: { id: "android-fixture" } } : undefined }
      } as ReturnType<typeof sdk.useOpenSecret>);
      const getService = spyOn(billing, "getBillingService");
      const search = spyOn(Route, "useSearch").mockReturnValue({ selected_plan: "pro" });
      cleanup.push(
        () => auth.mockRestore(),
        () => getService.mockRestore(),
        () => search.mockRestore()
      );
      act(() => {
        renderer = create(createElement(Route.options.component as ComponentType));
      });
      const content = JSON.stringify(renderer!.toJSON());
      expect(content).toContain("Purchases and plan changes are not available in the Android app");
      const hrefs = renderer!.root.findAllByType("a").map((link) => link.props.href as string);
      expect(hrefs).toContain(signedIn ? "/settings/billing" : "/login");
      expect(hrefs.includes("/redeem")).toBe(signedIn);
      expect(hrefs.some((href) => /^https?:/.test(href))).toBe(false);
      expect(search).not.toHaveBeenCalled();
      expect(getService).not.toHaveBeenCalled();
    });
  }
});
