import { useCallback, useEffect, useState } from "react";
import { storeKit, type StoreKitBridge, type StoreKitProduct } from "@/services/storeKitService";
import {
  appleProductIds,
  monthlyAppleProducts,
  monthlyAppleSizeProducts,
  type ApplePlan
} from "./applePricingPolicy";

type CatalogState = {
  products: Partial<Record<ApplePlan, StoreKitProduct>>;
  /** Valid products for the larger sizes billing lists, by App Store product. */
  sizes: Record<string, StoreKitProduct>;
  loading: boolean;
  error: string | null;
};

const noSizes: readonly string[] = [];
const idle: CatalogState = { products: {}, sizes: {}, loading: true, error: null };

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
  events: CatalogEvents = browserEvents,
  sizeIds: readonly string[] = noSizes
) {
  const [state, setState] = useState<CatalogState>(idle);
  const [attempt, setAttempt] = useState(0);
  // The caller derives this list on every render; reload only when it changes.
  const sizeKey = sizeIds.join("\n");

  useEffect(() => {
    let active = true;
    let generation = 0;
    let removeListener: (() => void) | undefined;
    let listening = false;
    const ids = appleProductIds(variant);
    const sizes = sizeKey ? sizeKey.split("\n") : [];
    const load = async () => {
      const request = ++generation;
      setState(idle);
      try {
        if (!ids) throw new Error("invalid_app_variant");
        const products = await bridge.getProducts([...Object.values(ids), ...sizes]);
        if (!active || request !== generation) return;
        const validated = monthlyAppleProducts(products, ids);
        setState({
          products: validated,
          // A size the App Store does not return is simply not offered.
          sizes: monthlyAppleSizeProducts(products, sizes),
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
            sizes: {},
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
          setState({
            products: {},
            sizes: {},
            loading: false,
            error: "Could not connect to the App Store."
          });
        }
      });
    return () => {
      active = false;
      generation++;
      events.visibility?.removeEventListener("visibilitychange", refreshOnResume);
      events.focus?.removeEventListener("focus", refreshOnResume);
      stopListening(removeListener);
    };
  }, [bridge, variant, attempt, events, sizeKey]);

  const retry = useCallback(() => setAttempt((value) => value + 1), []);
  return { ...state, retry };
}
