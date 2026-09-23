import { afterEach, describe, expect, mock, spyOn, test } from "bun:test";
import { isRedirect } from "@tanstack/react-router";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import * as platform from "@/utils/platform";
import { Route as RedeemRoute } from "@/routes/redeem";
import { PromoDialog } from "./PromoDialog";

const discount = {
  active: true as const,
  name: "Web offer",
  description: "A web subscription offer",
  percent_off: 25,
  starts_at: 1,
  expires_at: 2
};

let renderer: ReactTestRenderer | null = null;
const platformCleanups: Array<() => void> = [];
function setNativeIOS(value: boolean) {
  const spy = spyOn(platform, "isIOS").mockReturnValue(value);
  platformCleanups.push(() => spy.mockRestore());
}
afterEach(() => {
  if (renderer) act(() => renderer?.unmount());
  renderer = null;
  for (const cleanup of platformCleanups.splice(0).reverse()) cleanup();
});

describe("native iOS pass and promotion entry points", () => {
  test("redirects direct pass links before redemption content can load", () => {
    setNativeIOS(true);
    const beforeLoad = RedeemRoute.options.beforeLoad!;
    let result: unknown;
    try {
      beforeLoad({} as never);
    } catch (error) {
      result = error;
    }
    expect(isRedirect(result)).toBe(true);
    if (!isRedirect(result)) throw new Error("Expected a route redirect");
    expect(result.options.to).toBe("/pricing");
    expect(result.options.replace).toBe(true);
  });

  test("preserves the pass route for web, Android, and desktop", () => {
    setNativeIOS(false);
    expect(RedeemRoute.options.beforeLoad!({} as never)).toBeUndefined();
  });

  test("does not mount a percentage promotion on native iOS", () => {
    setNativeIOS(true);
    const onOpenChange = mock(() => {});
    act(() => {
      renderer = create(<PromoDialog open onOpenChange={onOpenChange} discount={discount} />);
    });
    expect(renderer!.toJSON()).toBeNull();
    expect(onOpenChange).not.toHaveBeenCalled();
  });

  test("keeps the existing promotion and its callback available outside native iOS", () => {
    setNativeIOS(false);
    const onOpenChange = mock(() => {});
    const view = PromoDialog({ open: true, onOpenChange, discount });
    expect(view).not.toBeNull();
    expect(view!.props.open).toBe(true);
    expect(view!.props.discount).toBe(discount);
    expect(view!.props.onOpenChange).toBe(onOpenChange);
  });
});
