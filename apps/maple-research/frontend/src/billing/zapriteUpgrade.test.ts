import { expect, test } from "bun:test";
import {
  isAllowedZapriteCheckoutUrl,
  zapritePaidPlanButtonText,
  zapriteUpgradeTarget
} from "./zapriteUpgrade";
import type { BillingStatus } from "./billingApi";

const zapritePro: BillingStatus = {
  is_subscribed: true,
  stripe_customer_id: null,
  product_id: "local-pro",
  product_name: "Pro",
  subscription_status: "active",
  current_period_end: null,
  can_chat: true,
  chats_remaining: null,
  payment_provider: "zaprite",
  total_tokens: null,
  used_tokens: null,
  usage_reset_date: null
};

test("eligible Zaprite Pro renders upgrade CTA rather than support email", () => {
  expect(zapritePaidPlanButtonText(zapritePro, { id: "local-max", name: "Max" }, false)).toBe(
    "Upgrade to Max"
  );
  expect(zapriteUpgradeTarget(zapritePro, { id: "local-max", name: "Max" })).toBe(true);
  expect(zapritePaidPlanButtonText(zapritePro, { id: "local-pro", name: "Pro" }, true)).toBe(
    "Start Chatting"
  );
  expect(zapritePaidPlanButtonText(zapritePro, { id: "team", name: "Team" }, false)).toBe(
    "Contact Us"
  );
});

test("any higher individual plan billing describes is an upgrade target", () => {
  const studio = {
    id: "local-studio",
    name: "Max Studio",
    plan_group: "individual",
    plan_rank: 3,
    display_name: "Studio"
  };
  const zapriteMax: BillingStatus = { ...zapritePro, product_id: "local-max", product_name: "Max" };
  for (const subscriber of [zapritePro, zapriteMax]) {
    expect(zapriteUpgradeTarget(subscriber, studio)).toBe(true);
    expect(zapritePaidPlanButtonText(subscriber, studio, false)).toBe("Upgrade to Studio");
  }

  // Never a lower or equal plan, a Team plan, or a Free subscriber.
  expect(zapritePaidPlanButtonText(zapriteMax, { id: "local-pro", name: "Pro" }, false)).toBe(
    "Contact Us"
  );
  expect(zapriteUpgradeTarget(zapriteMax, { id: "local-max", name: "Max" })).toBe(false);
  expect(zapriteUpgradeTarget(zapritePro, { ...studio, plan_group: "team", plan_rank: null })).toBe(
    false
  );
  expect(zapriteUpgradeTarget({ ...zapritePro, product_name: "Free" }, studio)).toBe(false);

  // Other payment providers change plans elsewhere.
  expect(
    zapritePaidPlanButtonText({ ...zapritePro, payment_provider: "stripe" }, studio, false)
  ).toBe(null);
});

test("checkout URL allowlist accepts Zaprite and local mock hosts", () => {
  expect(isAllowedZapriteCheckoutUrl("https://checkout.zaprite.com/abc")).toBe(true);
  expect(isAllowedZapriteCheckoutUrl("http://checkout.zaprite.com/abc")).toBe(false);
  expect(isAllowedZapriteCheckoutUrl("https://evil.example/abc")).toBe(false);
  expect(isAllowedZapriteCheckoutUrl("http://127.0.0.1:36683/v1/dev/zaprite-mock/checkout/x")).toBe(
    true
  );
});
