import { afterEach, describe, expect, mock, test } from "bun:test";
import type { ComponentProps } from "react";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { ApplePricing } from "./ApplePricing";
import type { AppleBillingPurchaseResult } from "@/billing/appleBillingSession";
import { AppleBillingApiError } from "@/billing/appleBillingApi";
import { appleBillingErrorMessage } from "@/billing/appleBillingLifecycle";
import { StoreKitRecoveryError } from "@/services/storeKitService";

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
  function mount(input: Props, scrollIntoView = mock(() => {})) {
    act(() => {
      renderer = create(<ApplePricing {...input} />, {
        createNodeMock: (element) => (element.type === "p" ? { scrollIntoView } : null)
      });
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
  test("explicit restore errors and successes are brought into view, with accurate failure wording", async () => {
    const input = props();
    const scrollIntoView = mock(() => {});
    input.restore = mock(async () => {
      throw new Error("sync_failed");
    });
    mount(input, scrollIntoView);
    expect(scrollIntoView).not.toHaveBeenCalled();
    await click("Restore purchases");
    expect(text(renderer!.root)).toContain("Purchases could not be restored");
    expect(text(renderer!.root.findByProps({ role: "alert" }))).not.toContain("cancelled");
    expect(text(renderer!.root)).not.toContain("Your purchase could not be confirmed");
    expect(scrollIntoView).toHaveBeenCalledWith({ block: "nearest" });
    expect(scrollIntoView).toHaveBeenCalledTimes(1);
    act(() => renderer!.update(<ApplePricing {...input} restore={async () => []} />));
    await click("Restore purchases");
    expect(text(renderer!.root)).toContain("No purchases were found");
    expect(scrollIntoView).toHaveBeenCalledTimes(2);
  });
  test("background recovery displays the sanitized ownership error without scrolling", () => {
    const input = props();
    const scrollIntoView = mock(() => {});
    const recoveryError = appleBillingErrorMessage(new AppleBillingApiError("conflict", 409));
    mount(input, scrollIntoView);
    act(() => renderer!.update(<ApplePricing {...input} recoveryError={recoveryError} />));
    expect(text(renderer!.root.findByProps({ role: "alert" }))).toBe(recoveryError);
    expect(text(renderer!.root)).toContain("belongs to another Maple account");
    expect(button("Subscribe to Pro").props.disabled).toBe(true);
    act(() => renderer!.update(<ApplePricing {...input} recoveryError={null} />));
    expect(scrollIntoView).not.toHaveBeenCalled();
  });
  for (const label of ["Restore purchases", "Retry purchases"]) {
    test(`${label} preserves a wrapped ownership conflict instead of suggesting Apple sign-in`, async () => {
      const input = props();
      const scrollIntoView = mock(() => {});
      const failure = new StoreKitRecoveryError(
        [],
        [
          { transactionId: "123", error: new AppleBillingApiError("conflict", 409) },
          { transactionId: "456", error: new Error("private-native-error-canary") }
        ]
      );
      const reject = async () => {
        throw failure;
      };
      input.restore = reject;
      input.retry = reject;
      mount(input, scrollIntoView);
      await click(label);
      const alert = text(renderer!.root.findByProps({ role: "alert" }));
      expect(alert).toBe(appleBillingErrorMessage(failure));
      expect(alert).toContain("belongs to another Maple account");
      expect(alert).not.toContain("sign-in prompt");
      expect(alert).not.toContain("private-native-error-canary");
      expect(alert).not.toContain("storekit_recovery_incomplete");
      expect(input.refreshStatus).not.toHaveBeenCalled();
      expect(scrollIntoView).toHaveBeenCalledTimes(1);
    });
  }
  for (const [label, action, expected] of [
    [
      "Manage Apple subscriptions",
      "manageApple",
      "Apple subscription management could not be opened"
    ],
    ["Apple Standard EULA", "openEula", "The Apple license agreement could not be opened"]
  ] as const) {
    test(`${label} retains safe operation-specific failure advice`, async () => {
      const input = props();
      input[action] = async () => {
        throw new Error("private-native-error-canary");
      };
      mount(input);
      await click(label);
      const alert = text(renderer!.root.findByProps({ role: "alert" }));
      expect(alert).toContain(expected);
      expect(alert).not.toContain("private-native-error-canary");
    });
  }
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
  for (const selected of [
    { payment_provider: "apple", product_name: "Pro", is_subscribed: true, current: true },
    { payment_provider: "apple", product_name: "Max", is_subscribed: true, current: false },
    { payment_provider: "stripe", product_name: "Pro", is_subscribed: true, current: false },
    { payment_provider: "apple", product_name: "Pro", is_subscribed: false, current: false }
  ] as const) {
    test(`acknowledgement reports current access accurately for ${selected.payment_provider} ${selected.product_name} subscribed=${selected.is_subscribed}`, async () => {
      const input = props();
      input.purchase = mock(async () => ({
        status: "success" as const,
        acknowledgement: {
          ...input.status!,
          acknowledged_transaction_id: "123",
          payment_provider: selected.payment_provider,
          product_name: selected.product_name,
          is_subscribed: selected.is_subscribed
        }
      }));
      mount(input);
      await click("Subscribe to Pro");
      const feedback = text(renderer!.root.findByProps({ role: "status" }));
      expect(feedback).toContain("Purchase confirmed.");
      if (selected.current) {
        expect(feedback).toContain("Maple Pro is your current plan.");
        expect(feedback).not.toContain("Review billing");
      } else {
        expect(feedback).toContain("Review billing to see your current plan");
        expect(feedback).not.toContain("Maple Pro is your current plan.");
      }
      expect(feedback).not.toContain("Your Maple plan has been updated");
      expect(input.purchase).toHaveBeenCalledWith(proId);
    });
  }
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

  // "Studio" is a fixture: the paywall learns a larger size only from billing.
  const maxId = "cloud.opensecret.maple.dev.max.monthly";
  const studioId = "app.example.studio.monthly";
  function withSize(): Props {
    const input = props();
    const max = { ...input.products.Pro!, productId: maxId, formattedPrice: "$114.99" };
    input.products = { ...input.products, Max: max };
    input.sizes = {
      Max: [
        {
          productId: studioId,
          planKey: "studio",
          catalogName: "Max Studio",
          displayName: "Studio",
          credits: "20× Pro's monthly credits",
          product: { ...max, productId: studioId, formattedPrice: "$229.99" }
        }
      ]
    };
    return input;
  }
  function sizeButtons() {
    const group = renderer!.root.findAll(
      (node) => node.props.role === "group" && node.props["aria-label"] === "Max sizes"
    )[0];
    return group.findAllByType("button");
  }
  test("a plan's larger size is one pick away and buys its own App Store product", async () => {
    const input = withSize();
    mount(input);
    expect(sizeButtons().map((node) => [text(node), node.props["aria-pressed"]])).toEqual([
      ["$114.99", true],
      ["$229.99", false]
    ]);
    expect(text(renderer!.root)).toContain("10× Pro's monthly credits");
    expect(button("Subscribe to Max").props.disabled).toBe(false);
    // The Pro card has no sizes and no switch.
    expect(
      renderer!.root.findAll((node) => node.props.role === "group" && !!node.props["aria-label"])
    ).toHaveLength(1);

    await act(async () => {
      sizeButtons()[1].props.onClick();
    });
    expect(sizeButtons().map((node) => node.props["aria-pressed"])).toEqual([false, true]);
    expect(text(renderer!.root)).toContain("Maple Studio");
    expect(text(renderer!.root)).toContain("20× Pro's monthly credits");
    expect(text(renderer!.root)).not.toContain("10× Pro's monthly credits");
    await click("Subscribe to Studio");
    expect(input.purchase).toHaveBeenCalledWith(studioId);

    await act(async () => {
      sizeButtons()[0].props.onClick();
    });
    await click("Subscribe to Max");
    expect(input.purchase).toHaveBeenLastCalledWith(maxId);
  });
  test("a subscriber on the larger size sees it selected and manages it; the plan itself stays buyable", async () => {
    const input = withSize();
    input.status = {
      ...input.status!,
      payment_provider: "apple",
      product_name: "Max Studio",
      plan_key: "studio"
    };
    mount(input);
    expect(sizeButtons().map((node) => node.props["aria-pressed"])).toEqual([false, true]);
    expect(text(renderer!.root)).toContain("Maple Studio");
    await click("Manage Apple subscription");
    expect(input.manageApple).toHaveBeenCalledTimes(1);
    expect(input.purchase).not.toHaveBeenCalled();
    await act(async () => {
      sizeButtons()[0].props.onClick();
    });
    expect(button("Subscribe to Max").props.disabled).toBe(false);
  });
  test("a size is not offered while its plan has no App Store price, and a purchase awaiting approval blocks both", async () => {
    const unpriced = withSize();
    unpriced.products = { Pro: unpriced.products.Pro };
    mount(unpriced);
    expect(
      renderer!.root.findAll((node) => node.props.role === "group" && !!node.props["aria-label"])
    ).toHaveLength(0);
    expect(button("Subscribe to Max").props.disabled).toBe(true);
    act(() => renderer?.unmount());

    const input = withSize();
    input.purchase = mock(async () => ({ status: "pending" as const }));
    mount(input);
    await act(async () => {
      sizeButtons()[1].props.onClick();
    });
    await click("Subscribe to Studio");
    expect(text(renderer!.root)).toContain("awaiting Apple's approval");
    expect(button("Subscribe to Studio").props.disabled).toBe(true);
    expect(button("Subscribe to Pro").props.disabled).toBe(true);
  });
});
