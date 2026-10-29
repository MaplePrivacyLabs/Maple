import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import * as platform from "@/utils/platform";
import { fetchPortalUrl } from "./billingApi";
import { billingPortalReturnOrigin, billingReturnOrigin } from "./returnOrigin";

describe("billing browser returns", () => {
  const previousBillingApi = process.env.VITE_MAPLE_BILLING_API_URL;
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
  const cleanup: Array<() => void> = [];

  beforeEach(() => {
    const native = spyOn(platform, "isTauri").mockReturnValue(true);
    const mobile = spyOn(platform, "isMobile").mockReturnValue(false);
    const android = spyOn(platform, "isAndroid").mockReturnValue(false);
    cleanup.push(
      () => native.mockRestore(),
      () => mobile.mockRestore(),
      () => android.mockRestore()
    );
    Object.defineProperty(globalThis, "window", {
      configurable: true,
      writable: true,
      value: { location: { origin: "http://tauri.localhost" } }
    });
  });

  afterEach(() => {
    cleanup
      .splice(0)
      .reverse()
      .forEach((restore) => restore());
    if (previousBillingApi === undefined) delete process.env.VITE_MAPLE_BILLING_API_URL;
    else process.env.VITE_MAPLE_BILLING_API_URL = previousBillingApi;
    if (previousWindow) Object.defineProperty(globalThis, "window", previousWindow);
    else Reflect.deleteProperty(globalThis, "window");
  });

  for (const [api, origin] of [
    ["https://billing-dev.opensecret.cloud", "https://app-dev.maple.ai"],
    ["https://billing-dev.opensecret.cloud/", "https://app-dev.maple.ai"],
    ["https://billing.opensecret.cloud", "https://app.maple.ai"]
  ]) {
    test(`desktop portal with ${api} returns directly to ${origin}`, async () => {
      process.env.VITE_MAPLE_BILLING_API_URL = api;
      const request = spyOn(globalThis, "fetch").mockResolvedValue(
        Response.json({ portal_url: "https://billing.stripe.com/p/session/test" })
      );
      cleanup.push(() => request.mockRestore());

      expect(billingReturnOrigin()).toBe(origin);
      await fetchPortalUrl("fixture-token");
      expect(request).toHaveBeenCalledTimes(1);
      expect(JSON.parse(String(request.mock.calls[0][1]?.body))).toEqual({
        return_url: origin
      });
    });
  }

  for (const origin of [
    "https://app.maple.ai",
    "https://app-dev.maple.ai",
    "https://app-dev.trymaple.ai",
    "http://127.0.0.1:5173"
  ]) {
    test(`web preserves its actual origin ${origin}`, () => {
      spyOn(platform, "isTauri").mockReturnValue(false);
      Object.defineProperty(window.location, "origin", { value: origin, configurable: true });
      process.env.VITE_MAPLE_BILLING_API_URL = "https://billing-dev.opensecret.cloud";
      expect(billingReturnOrigin()).toBe(origin);
      expect(billingPortalReturnOrigin()).toBe(origin);
    });
  }

  test("mobile keeps the existing associated-link host", () => {
    spyOn(platform, "isMobile").mockReturnValue(true);
    process.env.VITE_MAPLE_BILLING_API_URL = "https://billing-dev.opensecret.cloud";
    expect(billingReturnOrigin()).toBe("https://trymaple.ai");
  });

  for (const [api, origin] of [
    ["https://billing-dev.opensecret.cloud", "https://app-dev.maple.ai"],
    ["https://billing-dev.opensecret.cloud/", "https://app-dev.maple.ai"],
    ["https://billing.opensecret.cloud", "https://app.maple.ai"]
  ]) {
    test(`mobile portal returns directly to ${origin} without changing payment links`, async () => {
      spyOn(platform, "isMobile").mockReturnValue(true);
      process.env.VITE_MAPLE_BILLING_API_URL = api;
      const request = spyOn(globalThis, "fetch").mockResolvedValue(
        Response.json({ portal_url: "https://billing.stripe.com/p/session/test" })
      );
      cleanup.push(() => request.mockRestore());

      await fetchPortalUrl("fixture-token");
      expect(request).toHaveBeenCalledTimes(1);
      expect(JSON.parse(String(request.mock.calls[0][1]?.body))).toEqual({
        return_url: origin
      });
      expect(billingReturnOrigin()).toBe("https://trymaple.ai");
    });
  }
});
