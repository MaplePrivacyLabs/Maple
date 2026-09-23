import { afterEach, describe, expect, mock, test } from "bun:test";
import type { ComponentProps } from "react";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { ApplePricing } from "./ApplePricing";
import type { AppleBillingPurchaseResult } from "@/billing/appleBillingSession";
import { AppleBillingApiError } from "@/billing/appleBillingApi";

type Props = ComponentProps<typeof ApplePricing>;
const proId = "cloud.opensecret.maple.dev.pro.monthly";
function props(): Props {
  return {
    userId: "anonymous-account",
    isGuest: true,
    status: {
      is_subscribed: true,
      stripe_customer_id: null,
      product_id: "free",
      product_name: "Free",
      subscription_status: "active",
      current_period_end: null,
      can_chat: true,
      chats_remaining: 25,
      payment_provider: null,
      total_tokens: null,
      used_tokens: null,
      usage_reset_date: null,
      ios_iap_enabled: true
    },
    statusLoading: false,
    statusError: false,
    ready: true,
    busy: false,
    recoveryError: null,
    products: {
      Pro: {
        productId: proId,
        title: "Maple Pro",
        description: "Pro",
        formattedPrice: "23,00 €",
        priceCurrencyCode: "EUR",
        subscriptionPeriod: "P1M"
      }
    },
    productsLoading: false,
    productsError: null,
    purchase: mock(async () => ({ status: "cancelled" as const })),
    restore: mock(async () => []),
    retry: mock(async () => {}),
    reloadPrices: mock(() => {}),
    refreshStatus: mock(() => {}),
    manageApple: mock(async () => {}),
    openEula: mock(async () => {}),
    navigate: mock(() => {})
  };
}
function text(node: ReactTestInstance): string {
  return node.children.map((child) => (typeof child === "string" ? child : text(child))).join("");
}

describe("Apple pricing interactions", () => {
  let renderer: ReactTestRenderer | null = null;
  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    renderer = null;
  });
  function mount(input: Props) {
    act(() => {
      renderer = create(<ApplePricing {...input} />);
    });
  }
  function button(label: string) {
    const result = renderer!.root.findAllByType("button").find((node) => text(node) === label);
    if (!result) throw new Error(`Missing button ${label}`);
    return result;
  }
  async function click(label: string) {
    await act(async () => {
      await button(label).props.onClick();
    });
  }

  test("guest can buy without email; missing products have no fabricated price or purchase", async () => {
    const input = props();
    mount(input);
    expect(text(renderer!.root)).toContain("23,00 €");
    expect(text(renderer!.root)).toContain("Account ID: anonymous-account");
    expect(button("Subscribe to Pro").props.disabled).toBe(false);
    expect(button("Subscribe to Max").props.disabled).toBe(true);
    await click("Subscribe to Pro");
    expect(input.purchase).toHaveBeenCalledWith(proId);
    expect(text(renderer!.root)).toContain("Purchase cancelled");
  });
  test("new-purchase kill switch leaves restore, retry and manage operational", async () => {
    const input = props();
    input.status = { ...input.status!, ios_iap_enabled: false };
    mount(input);
    expect(button("Subscribe to Pro").props.disabled).toBe(true);
    await click("Restore purchases");
    await click("Retry purchases");
    await click("Manage Apple subscriptions");
    expect(input.restore).toHaveBeenCalledTimes(1);
    expect(input.retry).toHaveBeenCalledTimes(1);
    expect(input.manageApple).toHaveBeenCalledTimes(1);
    expect(input.purchase).not.toHaveBeenCalled();
  });
  test("missing flag and unknown or failed status cannot start a purchase", () => {
    for (const override of [
      { status: { ...props().status!, ios_iap_enabled: undefined } },
      { status: undefined },
      { statusLoading: true },
      { statusError: true },
      { ready: false },
      { recoveryError: "recovery pending" }
    ]) {
      mount({ ...props(), ...override });
      expect(button("Subscribe to Pro").props.disabled).toBe(true);
      act(() => renderer?.unmount());
      renderer = null;
    }
  });
  test("Team and another still-billing provider route to billing instead of checkout", async () => {
    const input = props();
    input.status = { ...input.status!, product_name: "Team", payment_provider: "stripe" };
    mount(input);
    expect(button("Subscribe to Pro").props.disabled).toBe(true);
    await click("Review billing");
    expect(input.navigate).toHaveBeenCalledWith("/settings/billing");
    expect(input.purchase).not.toHaveBeenCalled();
  });
  test("the existing Apple plan remains manageable with purchasing disabled", async () => {
    const input = props();
    input.status = {
      ...input.status!,
      product_name: "Pro",
      payment_provider: "apple",
      ios_iap_enabled: false
    };
    mount(input);
    await click("Manage Apple subscription");
    expect(input.manageApple).toHaveBeenCalledTimes(1);
    expect(input.purchase).not.toHaveBeenCalled();
  });
  test("pending approval prevents repeated purchases but allows recovery", async () => {
    const input = props();
    input.purchase = mock(async () => ({ status: "pending" as const }));
    mount(input);
    await click("Subscribe to Pro");
    expect(text(renderer!.root)).toContain("awaiting Apple's approval");
    expect(button("Subscribe to Pro").props.disabled).toBe(true);
    expect(button("Retry purchases").props.disabled).toBe(false);
  });
  test("double invocation cannot start two purchases before rerender", async () => {
    let finish!: (value: AppleBillingPurchaseResult) => void;
    const input = props();
    input.purchase = mock(
      () =>
        new Promise<AppleBillingPurchaseResult>((resolve) => {
          finish = resolve;
        })
    );
    mount(input);
    const buy = button("Subscribe to Pro").props.onClick;
    await act(async () => {
      buy();
      buy();
    });
    expect(input.purchase).toHaveBeenCalledTimes(1);
    await act(async () => {
      finish({ status: "cancelled" });
    });
  });
  test("provider busy transitions preserve the current purchase result", async () => {
    let finish!: (value: AppleBillingPurchaseResult) => void;
    const input = props();
    input.purchase = mock(
      () =>
        new Promise<AppleBillingPurchaseResult>((resolve) => {
          finish = resolve;
        })
    );
    mount(input);
    await click("Subscribe to Pro");
    act(() => renderer!.update(<ApplePricing {...input} busy />));
    act(() => renderer!.update(<ApplePricing {...input} busy={false} />));
    await act(async () => {
      finish({ status: "pending" });
    });
    expect(text(renderer!.root)).toContain("awaiting Apple's approval");
    expect(button("Subscribe to Pro").props.disabled).toBe(true);
  });
  for (const label of ["Restore purchases", "Retry purchases", "Manage Apple subscriptions"]) {
    test(`${label} cannot refresh the next account after unmount`, async () => {
      let finish!: () => void;
      const wait = new Promise<void>((resolve) => {
        finish = resolve;
      });
      const input = props();
      input.restore = async () => {
        await wait;
        return [];
      };
      input.retry = async () => {
        await wait;
      };
      input.manageApple = async () => {
        await wait;
      };
      mount(input);
      await click(label);
      act(() => renderer!.unmount());
      renderer = null;
      await act(async () => {
        finish();
      });
      expect(input.refreshStatus).not.toHaveBeenCalled();
    });
  }
  test("safe ownership error never prints the original native failure", async () => {
    const input = props();
    input.purchase = mock(async () => {
      throw new AppleBillingApiError("conflict", 409);
    });
    mount(input);
    await click("Subscribe to Pro");
    expect(text(renderer!.root)).toContain("belongs to another Maple account");
    expect(text(renderer!.root)).not.toContain("apple_billing_conflict");
  });
  test("late completion from an old credential callback cannot overwrite current feedback", async () => {
    let finish!: (value: AppleBillingPurchaseResult) => void;
    const input = props();
    input.purchase = mock(
      () =>
        new Promise<AppleBillingPurchaseResult>((resolve) => {
          finish = resolve;
        })
    );
    mount(input);
    await click("Subscribe to Pro");
    act(() =>
      renderer!.update(<ApplePricing {...input} purchase={async () => ({ status: "cancelled" })} />)
    );
    await act(async () => {
      finish({ status: "pending" });
    });
    expect(text(renderer!.root)).not.toContain("awaiting Apple's approval");
    expect(button("Subscribe to Pro").props.disabled).toBe(false);
  });
});
