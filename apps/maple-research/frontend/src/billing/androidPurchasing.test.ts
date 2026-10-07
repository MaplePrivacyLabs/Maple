import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import * as platform from "@/utils/platform";
import {
  createCheckoutSession,
  createZapriteCheckoutSession,
  createZapriteUpgrade,
  createZapriteUpgradeQuote,
  fetchApiCreditBalance,
  fetchBillingStatus,
  fetchPortalUrl,
  fetchTeamStatus,
  openValidatedCheckoutUrl,
  purchaseApiCredits,
  purchaseApiCreditsZaprite
} from "./billingApi";

describe("Android consumption-only billing boundary", () => {
  let android: ReturnType<typeof spyOn<typeof platform, "isAndroid">>;
  let request: ReturnType<typeof spyOn<typeof globalThis, "fetch">>;

  beforeEach(() => {
    android = spyOn(platform, "isAndroid").mockReturnValue(true);
    request = spyOn(globalThis, "fetch").mockImplementation(
      Object.assign(
        async () => Response.json({ product_name: "Pro", can_chat: true, balance: 12345 }),
        { preconnect: globalThis.fetch.preconnect }
      )
    );
  });
  afterEach(() => {
    request.mockRestore();
    android.mockRestore();
  });

  const blocked: Array<[string, () => Promise<unknown>]> = [
    ["card checkout", () => createCheckoutSession("token", "a@example.test", "pro", "/", "/")],
    ["Bitcoin checkout", () => createZapriteCheckoutSession("token", "a@example.test", "pro", "/")],
    ["card portal", () => fetchPortalUrl("token")],
    ["Bitcoin quote", () => createZapriteUpgradeQuote("token", "max")],
    ["Bitcoin upgrade", () => createZapriteUpgrade("token", "quote", "idempotency-key")],
    ["existing checkout URL", () => openValidatedCheckoutUrl("https://pay.zaprite.com/checkout")],
    [
      "card credits",
      () =>
        purchaseApiCredits("token", {
          credits: 10000,
          email: "a@example.test",
          success_url: "/",
          cancel_url: "/"
        })
    ],
    [
      "Bitcoin credits",
      () =>
        purchaseApiCreditsZaprite("token", {
          credits: 10000,
          email: "a@example.test",
          success_url: "/"
        })
    ]
  ];

  for (const [name, action] of blocked) {
    test(`${name} is blocked before requesting or opening payment`, async () => {
      await expect(action()).rejects.toThrow("Purchases are not available in the Android app.");
      expect(request).not.toHaveBeenCalled();
    });
  }

  test("existing subscription, credits and team reads still reach billing", async () => {
    expect((await fetchBillingStatus("token")).can_chat).toBe(true);
    expect((await fetchApiCreditBalance("token")).balance).toBe(12345);
    await fetchTeamStatus("token");
    expect(request).toHaveBeenCalledTimes(3);
  });
});
