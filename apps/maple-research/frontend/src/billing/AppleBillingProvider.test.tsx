import { expect, test } from "bun:test";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { AppleBillingRuntime } from "./AppleBillingProvider";
import { AppleBillingLifecycle } from "./appleBillingLifecycle";
import {
  AppleBillingSessionChangedError,
  type AppleBillingPurchaseResult
} from "./appleBillingSession";
import { useAppleBilling, type AppleBillingContextValue } from "./useAppleBilling";

test("provider preserves current purchase feedback across busy updates and revokes old owner actions", async () => {
  let revision = 1;
  let resolvePurchase!: (value: AppleBillingPurchaseResult) => void;
  let purchases = 0;
  let seen!: AppleBillingContextValue;
  let destroyed = 0;
  let observerCleanup = 0;
  const lifecycle = new AppleBillingLifecycle({
    readIdentity: () => ({ apiOrigin: "https://auth.example.test", principalId: "a", revision }),
    createSession: () => {
      const ownerRevision = revision;
      let disposed = false;
      return {
        assertCurrent: () => {
          if (disposed || ownerRevision !== revision) throw new AppleBillingSessionChangedError();
        },
        dispose: () => {
          disposed = true;
          destroyed++;
        },
        start: async () => [],
        restore: async () => [],
        purchase: () => {
          purchases++;
          return new Promise<AppleBillingPurchaseResult>((resolve) => {
            resolvePurchase = resolve;
          });
        }
      };
    },
    onAcknowledged: () => {}
  });
  function Probe() {
    seen = useAppleBilling();
    return <p>{seen.busy ? "busy" : "idle"}</p>;
  }
  const observe = () => () => {
    observerCleanup++;
  };
  const reset = () => {};
  let renderer!: ReactTestRenderer;
  await act(async () => {
    renderer = create(
      <AppleBillingRuntime
        lifecycle={lifecycle}
        userId="a"
        onAccountChange={reset}
        observe={observe}
      >
        <Probe />
      </AppleBillingRuntime>
    );
  });
  expect(seen.ready).toBe(true);
  const owner = seen.ownerKey;
  const purchase = seen.purchase;
  let pending!: Promise<AppleBillingPurchaseResult>;
  act(() => {
    pending = seen.purchase("pro");
  });
  expect(seen.busy).toBe(true);
  expect(seen.purchase).toBe(purchase);
  expect(seen.ownerKey).toBe(owner);
  await act(async () => {
    resolvePurchase({ status: "pending" });
    await pending;
  });
  await expect(pending).resolves.toEqual({ status: "pending" });
  expect(seen.busy).toBe(false);
  expect(seen.purchase).toBe(purchase);
  await act(async () => {
    revision++;
    lifecycle.tick();
  });
  expect(seen.ownerKey).not.toBe(owner);
  expect(seen.purchase).not.toBe(purchase);
  await expect(purchase("pro")).rejects.toBeInstanceOf(AppleBillingSessionChangedError);
  expect(purchases).toBe(1);
  act(() => renderer.unmount());
  expect(destroyed).toBe(2);
  expect(observerCleanup).toBe(1);
});
