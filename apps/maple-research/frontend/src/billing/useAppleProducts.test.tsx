import { afterEach, describe, expect, test } from "bun:test";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import type { StoreKitBridge, StoreKitProduct } from "@/services/storeKitService";
import { useAppleProducts } from "./useAppleProducts";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
const product: StoreKitProduct = {
  productId: "cloud.opensecret.maple.dev.pro.monthly",
  title: "Pro",
  description: "Monthly",
  formattedPrice: "$23.00",
  priceCurrencyCode: "USD",
  subscriptionPeriod: "P1M"
};

describe("Apple price lifecycle", () => {
  let renderer: ReactTestRenderer | null = null;
  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    renderer = null;
  });
  function fixture() {
    let change!: (country: string | null) => void;
    let removed = 0;
    const requests: ReturnType<typeof deferred<StoreKitProduct[]>>[] = [];
    const visibility = Object.assign(new EventTarget(), {
      visibilityState: "visible" as DocumentVisibilityState
    });
    const events = { visibility, focus: new EventTarget() };
    let view!: ReturnType<typeof useAppleProducts>;
    const bridge: Pick<StoreKitBridge, "getProducts" | "onStorefront"> = {
      getProducts: () => {
        const request = deferred<StoreKitProduct[]>();
        requests.push(request);
        return request.promise;
      },
      onStorefront: async (callback) => {
        change = callback;
        return () => {
          removed++;
        };
      }
    };
    function Harness() {
      view = useAppleProducts("dev", bridge, events);
      return null;
    }
    return {
      Harness,
      requests,
      events,
      change: () => change("FRA"),
      get view() {
        return view;
      },
      get removed() {
        return removed;
      }
    };
  }
  test("a storefront change immediately removes old prices and rejects an older completion", async () => {
    const f = fixture();
    await act(async () => {
      renderer = create(<f.Harness />);
    });
    await act(async () => {
      f.requests[0].resolve([product]);
    });
    expect(f.view.products.Pro?.formattedPrice).toBe("$23.00");
    await act(async () => {
      f.change();
    });
    expect(f.view.products).toEqual({});
    expect(f.view.loading).toBe(true);
    await act(async () => {
      f.change();
    });
    await act(async () => {
      f.requests[2].resolve([{ ...product, formattedPrice: "23,00 €", priceCurrencyCode: "EUR" }]);
    });
    await act(async () => {
      f.requests[1].resolve([product]);
    });
    expect(f.view.products.Pro?.formattedPrice).toBe("23,00 €");
  });
  test("failure is recoverable and listener resources are disposed on retry and unmount", async () => {
    const f = fixture();
    await act(async () => {
      renderer = create(<f.Harness />);
    });
    await act(async () => {
      f.requests[0].reject(new Error("secret native error"));
    });
    expect(f.view.products).toEqual({});
    expect(f.view.error).not.toContain("secret");
    await act(async () => {
      f.view.retry();
    });
    expect(f.removed).toBe(1);
    await act(async () => {
      f.requests[1].resolve([product]);
    });
    expect(f.view.products.Pro).toEqual(product);
    act(() => renderer!.unmount());
    renderer = null;
    expect(f.removed).toBe(2);
  });
  test("late product completion after unmount does not publish state", async () => {
    const f = fixture();
    await act(async () => {
      renderer = create(<f.Harness />);
    });
    act(() => renderer!.unmount());
    renderer = null;
    await act(async () => {
      f.requests[0].resolve([product]);
    });
    expect(f.view.products).toEqual({});
    expect(f.removed).toBe(1);
  });
  test("returning to the app refreshes prices and removes foreground listeners on teardown", async () => {
    const f = fixture();
    await act(async () => {
      renderer = create(<f.Harness />);
    });
    await act(async () => {
      f.requests[0].resolve([product]);
    });
    await act(async () => {
      f.events.visibility.visibilityState = "hidden";
      f.events.visibility.dispatchEvent(new Event("visibilitychange"));
    });
    expect(f.requests).toHaveLength(1);
    await act(async () => {
      f.events.visibility.visibilityState = "visible";
      f.events.visibility.dispatchEvent(new Event("visibilitychange"));
    });
    expect(f.requests).toHaveLength(2);
    expect(f.view.products).toEqual({});
    await act(async () => {
      f.events.focus.dispatchEvent(new Event("focus"));
    });
    expect(f.requests).toHaveLength(3);
    act(() => renderer!.unmount());
    renderer = null;
    f.events.focus.dispatchEvent(new Event("focus"));
    f.events.visibility.dispatchEvent(new Event("visibilitychange"));
    expect(f.requests).toHaveLength(3);
  });
  test("an unresolved product request can be replaced by explicit reload", async () => {
    const f = fixture();
    await act(async () => {
      renderer = create(<f.Harness />);
    });
    await act(async () => {
      f.view.retry();
    });
    await act(async () => {
      f.requests[1].resolve([product]);
    });
    expect(f.view.products.Pro).toEqual(product);
    await act(async () => {
      f.requests[0].resolve([]);
    });
    expect(f.view.products.Pro).toEqual(product);
  });
  for (const late of [false, true]) {
    test(`consumes rejected async unregister after ${late ? "late registration" : "normal teardown"}`, async () => {
      const registration = deferred<() => void>();
      const bridge: Pick<StoreKitBridge, "getProducts" | "onStorefront"> = {
        getProducts: async () => [],
        onStorefront: () => registration.promise
      };
      let removed = 0;
      const remove = async () => {
        removed++;
        throw new Error("native unregister unavailable");
      };
      function Harness() {
        useAppleProducts("dev", bridge);
        return null;
      }
      await act(async () => {
        renderer = create(<Harness />);
      });
      if (!late)
        await act(async () => {
          registration.resolve(remove);
        });
      act(() => renderer!.unmount());
      renderer = null;
      if (late)
        await act(async () => {
          registration.resolve(remove);
        });
      await act(async () => {});
      expect(removed).toBe(1);
    });
  }
});
