import { afterEach, describe, expect, mock, test } from "bun:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import type { BillingStatus, BillingSubscription } from "@/billing/billingApi";
import type { AppleBillingContextValue } from "@/billing/useAppleBilling";
import { BillingSubscriptions } from "./BillingSubscriptions";
import { ApplePurchaseRecovery } from "./ApplePurchaseRecovery";

function textContent(node: ReactTestInstance): string {
  return node.children
    .map((child) => (typeof child === "string" ? child : textContent(child)))
    .join("");
}

const appleSubscription: BillingSubscription = {
  provider: "apple",
  plan: "Pro",
  state: "active",
  renews_at: null,
  manage: "app_store",
  selected: false,
  environment: "Sandbox",
  auto_renew_enabled: true
};
function status(subscriptions: BillingSubscription[]): BillingStatus {
  return {
    is_subscribed: true,
    stripe_customer_id: "cus_fixture",
    product_id: "max",
    product_name: "Max",
    subscription_status: "active",
    current_period_end: null,
    can_chat: true,
    chats_remaining: null,
    payment_provider: "stripe",
    total_tokens: 100,
    used_tokens: 0,
    usage_reset_date: null,
    ios_iap_enabled: false,
    subscriptions,
    conflict: { other_provider: "stripe", action: "review_subscriptions" }
  };
}

function recovery(overrides: Partial<AppleBillingContextValue> = {}): AppleBillingContextValue {
  return {
    ownerKey: "test-session-1",
    available: true,
    ready: true,
    busy: false,
    recoveryError: null,
    purchase: mock(async () => ({ status: "cancelled" as const })),
    restore: mock(async () => []),
    retry: mock(async () => {}),
    ...overrides
  };
}

describe("billing subscription settings", () => {
  let renderer: ReactTestRenderer | null = null;
  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    renderer = null;
  });

  test("renders selected Stripe and unselected Apple management with sandbox/conflict labels", async () => {
    const manageApple = mock(async () => {});
    const manageStripe = mock(async () => {});
    act(() => {
      renderer = create(
        <BillingSubscriptions
          status={status([
            {
              ...appleSubscription,
              provider: "stripe",
              plan: "Max",
              manage: "portal",
              selected: true,
              environment: undefined
            },
            appleSubscription
          ])}
          manageApple={manageApple}
          manageStripe={manageStripe}
        />
      );
    });
    expect(textContent(renderer!.root)).toContain("Supplies your current plan");
    expect(textContent(renderer!.root)).toContain("Apple sandbox test");
    expect(textContent(renderer!.root)).toContain(
      "does not cancel another provider's subscription"
    );
    const apple = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button) === "Manage with Apple");
    const stripe = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button) === "Manage card subscription");
    expect(apple?.props.disabled).toBe(false);
    expect(stripe?.props.disabled).toBe(false);
    await act(async () => {
      apple!.props.onClick();
    });
    expect(manageApple).toHaveBeenCalledTimes(1);
    expect(manageStripe).toHaveBeenCalledTimes(0);
    await act(async () => {
      stripe!.props.onClick();
    });
    expect(manageStripe).toHaveBeenCalledTimes(1);
  });

  test("does not offer a Stripe portal to team members and explains prepaid subscriptions", () => {
    act(() => {
      renderer = create(
        <BillingSubscriptions
          status={status([
            {
              ...appleSubscription,
              provider: "stripe",
              plan: "Team",
              manage: "team_admin",
              selected: true
            },
            { ...appleSubscription, provider: "zaprite", manage: "zaprite" },
            { ...appleSubscription, provider: "subscription_pass", manage: "subscription_pass" }
          ])}
        />
      );
    });
    expect(renderer!.root.findAllByType("button").map(textContent)).toEqual(["Contact support"]);
    expect(textContent(renderer!.root)).toContain(
      "Your team administrator manages this subscription"
    );
    expect(textContent(renderer!.root)).toContain("Paid with Bitcoin");
    expect(textContent(renderer!.root)).toContain("This pass does not renew automatically");
  });

  test("reports bounded management failures and permits another attempt", async () => {
    const manageApple = mock(async () => {
      throw new Error("private-native-error-canary");
    });
    act(() => {
      renderer = create(
        <BillingSubscriptions status={status([appleSubscription])} manageApple={manageApple} />
      );
    });
    await act(async () => {
      renderer!.root.findByType("button").props.onClick();
    });
    expect(textContent(renderer!.root)).toContain("Unable to open subscription management");
    expect(textContent(renderer!.root)).not.toContain("private-native-error-canary");
    expect(renderer!.root.findByType("button").props.disabled).toBe(false);
  });

  test("restore remains available without a subscription and reports only completed verification", async () => {
    const apple = recovery();
    act(() => {
      renderer = create(<ApplePurchaseRecovery apple={apple} />);
    });
    const restore = renderer!.root.findByType("button");
    expect(restore.props.disabled).toBe(false);
    await act(async () => {
      restore.props.onClick();
    });
    expect(apple.restore).toHaveBeenCalledTimes(1);
    expect(textContent(renderer!.root)).toContain("Apple purchases checked");
    expect(textContent(renderer!.root)).not.toContain("Pro unlocked");
  });

  test("recovery failures offer explicit retry without initiating Apple sync again", async () => {
    const apple = recovery({
      recoveryError: "Your purchases are saved by Apple; try again when connected."
    });
    act(() => {
      renderer = create(<ApplePurchaseRecovery apple={apple} />);
    });
    const retry = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button) === "Retry confirmation");
    await act(async () => {
      retry!.props.onClick();
    });
    expect(apple.retry).toHaveBeenCalledTimes(1);
    expect(apple.restore).toHaveBeenCalledTimes(0);
  });

  test("a refreshed session does not inherit a previous restore's success or failure", async () => {
    for (const fails of [false, true]) {
      let finish!: () => void;
      const pending = new Promise<never[]>((resolve, reject) => {
        finish = () => (fails ? reject(new Error("old-session-error-canary")) : resolve([]));
      });
      const oldSession = recovery({
        ownerKey: "same-user-session-1",
        restore: async () => pending
      });
      act(() => {
        renderer = create(<ApplePurchaseRecovery key={oldSession.ownerKey} apple={oldSession} />);
      });
      const oldRestore = renderer!.root.findByType("button").props.onClick;
      act(() => {
        oldRestore();
      });
      const newSession = recovery({ ownerKey: "same-user-session-2" });
      act(() => {
        renderer!.update(<ApplePurchaseRecovery key={newSession.ownerKey} apple={newSession} />);
      });
      await act(async () => {
        finish();
        await pending.catch(() => {});
      });
      expect(textContent(renderer!.root)).not.toContain("Apple purchases checked");
      expect(textContent(renderer!.root)).not.toContain("couldn't confirm");
      expect(renderer!.root.findByType("button").props.disabled).toBe(false);
      // A detached button callback is inert after its owner has unmounted.
      act(() => {
        oldRestore();
      });
      expect(newSession.restore).toHaveBeenCalledTimes(0);
      act(() => renderer?.unmount());
      renderer = null;
    }
  });

  test("restore is unavailable on other platforms and disabled while the account is not ready", () => {
    act(() => {
      renderer = create(<ApplePurchaseRecovery apple={recovery({ available: false })} />);
    });
    expect(renderer!.toJSON()).toBeNull();
    act(() => {
      renderer!.update(<ApplePurchaseRecovery apple={recovery({ ready: false })} />);
    });
    expect(renderer!.root.findByType("button").props.disabled).toBe(true);
    act(() => {
      renderer!.update(<ApplePurchaseRecovery apple={recovery({ busy: true })} />);
    });
    expect(renderer!.root.findByType("button").props.disabled).toBe(true);
  });
});
