import { useCallback, useEffect, useState } from "react";
import { storeKit, type StoreKitBridge, type StoreKitProduct } from "@/services/storeKitService";
import { appleProductIds, monthlyAppleProducts, type ApplePlan } from "./applePricingPolicy";

type CatalogState = {
  products: Partial<Record<ApplePlan, StoreKitProduct>>;
  loading: boolean;
  error: string | null;
};

interface CatalogEvents {
  visibility?: EventTarget & { visibilityState: DocumentVisibilityState };
  focus?: EventTarget;
}

const browserEvents: CatalogEvents = {
  visibility: typeof document === "undefined" ? undefined : document,
  focus: typeof window === "undefined" ? undefined : window
};

function stopListening(remove: (() => void) | undefined): void {
  try {
    // Native listener cleanup can return a Promise despite the bridge's void
    // callback type. A late unregister failure must not become unhandled.
    void Promise.resolve(remove?.()).catch(() => {});
  } catch {
    // Teardown is best effort; all callbacks are fenced by active/generation.
  }
}

export function useAppleProducts(
  variant: string | undefined,
  bridge: Pick<StoreKitBridge, "getProducts" | "onStorefront"> = storeKit,
  events: CatalogEvents = browserEvents
) {
  const [state, setState] = useState<CatalogState>({ products: {}, loading: true, error: null });
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let active = true;
    let generation = 0;
    let removeListener: (() => void) | undefined;
    let listening = false;
    const ids = appleProductIds(variant);
    const load = async () => {
      const request = ++generation;
      setState({ products: {}, loading: true, error: null });
      try {
        if (!ids) throw new Error("invalid_app_variant");
        const products = await bridge.getProducts(Object.values(ids));
        if (!active || request !== generation) return;
        const validated = monthlyAppleProducts(products, ids);
        setState({
          products: validated,
          loading: false,
          error:
            Object.keys(validated).length === 2
              ? null
              : "Some plans are unavailable from the App Store. Try reloading prices."
        });
      } catch {
        if (active && request === generation) {
          setState({
            products: {},
            loading: false,
            error: "App Store prices could not be loaded. Try again when you are connected."
          });
        }
      }
    };
    const refreshOnResume = () => {
      if (active && listening && events.visibility?.visibilityState !== "hidden") void load();
    };
    events.visibility?.addEventListener("visibilitychange", refreshOnResume);
    events.focus?.addEventListener("focus", refreshOnResume);
    // Install the listener first so a storefront change cannot leave an old price
    // actionable between the initial request and event registration.
    void bridge
      .onStorefront(() => {
        if (active) void load();
      })
      .then((remove) => {
        if (!active) {
          stopListening(remove);
          return;
        }
        removeListener = remove;
        listening = true;
        void load();
      })
      .catch(() => {
        if (active) {
          setState({ products: {}, loading: false, error: "Could not connect to the App Store." });
        }
      });
    return () => {
      active = false;
      generation++;
      events.visibility?.removeEventListener("visibilitychange", refreshOnResume);
      events.focus?.removeEventListener("focus", refreshOnResume);
      stopListening(removeListener);
    };
  }, [bridge, variant, attempt, events]);

  const retry = useCallback(() => setAttempt((value) => value + 1), []);
  return { ...state, retry };
}
