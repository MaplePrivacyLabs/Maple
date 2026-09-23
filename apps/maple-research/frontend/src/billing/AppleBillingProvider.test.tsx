import { expect, mock, spyOn, test } from "bun:test";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { AppleBillingRuntime } from "./AppleBillingProvider";
import { AppleBillingLifecycle } from "./appleBillingLifecycle";
import {
  AppleBillingSessionChangedError,
  type AppleBillingPurchaseResult
} from "./appleBillingSession";
import { useAppleBilling, type AppleBillingContextValue } from "./useAppleBilling";
import { AppleBillingApiError } from "./appleBillingApi";
import { StoreKitRecoveryError } from "@/services/storeKitService";

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

for (const status of [400, 409] as const) {
  test(`default observer preserves ${status} suppression on every wake and removes its listeners`, async () => {
    const priorWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
    const priorDocument = Object.getOwnPropertyDescriptor(globalThis, "document");
    const windowEvents = new EventTarget();
    const documentEvents = new EventTarget();
    let interval!: () => void;
    const timer = 17;
    const fakeWindow = {
      setInterval: mock((callback: () => void) => {
        interval = callback;
        return timer;
      }),
      clearInterval: mock(() => {}),
      addEventListener: mock(windowEvents.addEventListener.bind(windowEvents)),
      removeEventListener: mock(windowEvents.removeEventListener.bind(windowEvents))
    };
    const fakeDocument = {
      visibilityState: "visible",
      addEventListener: mock(documentEvents.addEventListener.bind(documentEvents)),
      removeEventListener: mock(documentEvents.removeEventListener.bind(documentEvents))
    };
    Object.defineProperty(globalThis, "window", { configurable: true, value: fakeWindow });
    Object.defineProperty(globalThis, "document", { configurable: true, value: fakeDocument });

    let starts = 0;
    let submissions = 0;
    let ownershipResolved = false;
    let seen!: AppleBillingContextValue;
    const lifecycle = new AppleBillingLifecycle({
      readIdentity: () => ({
        apiOrigin: "https://auth.example.test",
        principalId: "a",
        revision: 1
      }),
      createSession: (_owner, _acknowledged, _listenerError, retryPolicy) => ({
        assertCurrent: () => {},
        dispose: () => {},
        start: async () => {
          starts++;
          retryPolicy.observe("123", "unchanged-signed-transaction");
          try {
            retryPolicy.assertMaySubmit("123", "unchanged-signed-transaction");
            submissions++;
            if (!ownershipResolved) {
              throw new AppleBillingApiError(
                status === 409 ? "conflict" : "invalid_transaction",
                status
              );
            }
            return [];
          } catch (error) {
            retryPolicy.failed("123", "unchanged-signed-transaction", error);
            throw new StoreKitRecoveryError([], [{ transactionId: "123", error }]);
          }
        },
        restore: async () => [],
        purchase: async () => ({ status: "cancelled" as const })
      }),
      onAcknowledged: () => {}
    });
    const automatic = spyOn(lifecycle, "recoverAutomatically");
    const explicit = spyOn(lifecycle, "retry");
    const tick = spyOn(lifecycle, "tick");
    function Probe() {
      seen = useAppleBilling();
      return null;
    }
    let renderer: ReactTestRenderer | undefined;
    try {
      await act(async () => {
        renderer = create(
          <AppleBillingRuntime lifecycle={lifecycle} userId="a" onAccountChange={() => {}}>
            <Probe />
          </AppleBillingRuntime>
        );
      });
      expect(submissions).toBe(1);
      expect(starts).toBe(1);
      expect(seen.recoveryError).not.toBeNull();
      expect(fakeWindow.setInterval).toHaveBeenCalledWith(lifecycle.tick, 1_000);
      const ticksBeforeInterval = tick.mock.calls.length;
      act(() => interval());
      expect(tick).toHaveBeenCalledTimes(ticksBeforeInterval + 1);
      expect(starts).toBe(1);

      const wakeEvents = [
        [windowEvents, "focus"],
        [windowEvents, "online"],
        [windowEvents, "pageshow"],
        [documentEvents, "visibilitychange"]
      ] as const;
      for (const [target, event] of wakeEvents) {
        await act(async () => {
          target.dispatchEvent(new Event(event));
        });
      }
      expect(automatic).toHaveBeenCalledTimes(4);
      expect(explicit).not.toHaveBeenCalled();
      expect(starts).toBe(5);
      expect(submissions).toBe(1);
      expect(seen.recoveryError).not.toBeNull();

      fakeDocument.visibilityState = "hidden";
      for (const [target, event] of wakeEvents) {
        await act(async () => {
          target.dispatchEvent(new Event(event));
        });
      }
      expect(automatic).toHaveBeenCalledTimes(4);
      expect(starts).toBe(5);
      fakeDocument.visibilityState = "visible";

      // A support/configuration correction may now allow the same signed input;
      // only the user's explicit retry lifts the prior automatic submission block.
      ownershipResolved = true;
      await act(async () => {
        await seen.retry();
      });
      expect(explicit).toHaveBeenCalledTimes(1);
      expect(submissions).toBe(2);
      expect(seen.recoveryError).toBeNull();

      act(() => renderer?.unmount());
      renderer = undefined;
      expect(fakeWindow.clearInterval).toHaveBeenCalledWith(timer);
      expect(fakeWindow.removeEventListener).toHaveBeenCalledTimes(3);
      expect(fakeDocument.removeEventListener).toHaveBeenCalledTimes(1);
      for (const [event, listener] of fakeWindow.addEventListener.mock.calls) {
        expect(fakeWindow.removeEventListener).toHaveBeenCalledWith(event, listener);
      }
      for (const [event, listener] of fakeDocument.addEventListener.mock.calls) {
        expect(fakeDocument.removeEventListener).toHaveBeenCalledWith(event, listener);
      }
      for (const [target, event] of wakeEvents) target.dispatchEvent(new Event(event));
      expect(automatic).toHaveBeenCalledTimes(4);
      expect(explicit).toHaveBeenCalledTimes(1);
    } finally {
      if (renderer) act(() => renderer?.unmount());
      automatic.mockRestore();
      explicit.mockRestore();
      tick.mockRestore();
      if (priorWindow) Object.defineProperty(globalThis, "window", priorWindow);
      else Reflect.deleteProperty(globalThis, "window");
      if (priorDocument) Object.defineProperty(globalThis, "document", priorDocument);
      else Reflect.deleteProperty(globalThis, "document");
    }
  });
}
