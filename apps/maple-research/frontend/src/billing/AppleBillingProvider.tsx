import {
  useEffect,
  useLayoutEffect,
  useCallback,
  useMemo,
  useRef,
  useSyncExternalStore,
  type ReactNode
} from "react";
import { readNativeUserAuth, useOpenSecret } from "@mapleai/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { useBillingState } from "@/state/useLocalState";
import { isIOS } from "@/utils/platform";
import { createAppleBillingSession } from "./appleBillingSession";
import { AppleBillingLifecycle, registerAppleBillingLifecycle } from "./appleBillingLifecycle";
import { AppleBillingContext } from "./useAppleBilling";

function IOSAppleBillingProvider({ children }: { children: ReactNode }) {
  const os = useOpenSecret();
  const osRef = useRef(os);
  osRef.current = os;
  const userId = os.auth.user?.user.id ?? null;
  const queryClient = useQueryClient();
  const { setBillingStatus } = useBillingState();
  const lifecycle = useMemo(
    () =>
      new AppleBillingLifecycle({
        readIdentity: () => {
          const { apiOrigin, revision, principalId } = readNativeUserAuth(
            import.meta.env.VITE_OPEN_SECRET_API_URL
          );
          return { apiOrigin, revision, principalId };
        },
        createSession: (owner, onAcknowledged, onListenerError) =>
          createAppleBillingSession({
            userId: owner,
            openSecretApiUrl: import.meta.env.VITE_OPEN_SECRET_API_URL,
            billingUrl: import.meta.env.VITE_MAPLE_BILLING_API_URL ?? "",
            auth: osRef.current,
            onAcknowledged,
            onListenerError,
            allowInsecureLoopback: import.meta.env.DEV
          }),
        onAcknowledged: (status, owner) => {
          setBillingStatus(status);
          queryClient.setQueryData(["billingStatus", owner], status);
          // Refetch legacy readers and usage displays; none may assume the Apple
          // purchase is the selected plan (a Team or another provider can win).
          void queryClient.invalidateQueries({ queryKey: ["billingStatus"] });
          void queryClient.invalidateQueries({ queryKey: ["apiCreditBalance"] });
        }
      }),
    [queryClient, setBillingStatus]
  );
  const resetBilling = useCallback(() => setBillingStatus(null), [setBillingStatus]);
  return (
    <AppleBillingRuntime lifecycle={lifecycle} userId={userId} onAccountChange={resetBilling}>
      {children}
    </AppleBillingRuntime>
  );
}

function observeRecovery(lifecycle: AppleBillingLifecycle): () => void {
  // SDK exposes credential snapshots rather than refresh subscriptions. This
  // check is local; session methods also fence every asynchronous boundary.
  const timer = window.setInterval(lifecycle.tick, 1_000);
  const recover = () => {
    lifecycle.tick();
    if (document.visibilityState !== "hidden" && lifecycle.getSnapshot().ready) {
      void lifecycle.retry().catch(() => {});
    }
  };
  window.addEventListener("online", recover);
  window.addEventListener("focus", recover);
  window.addEventListener("pageshow", recover);
  document.addEventListener("visibilitychange", recover);
  return () => {
    window.clearInterval(timer);
    window.removeEventListener("online", recover);
    window.removeEventListener("focus", recover);
    window.removeEventListener("pageshow", recover);
    document.removeEventListener("visibilitychange", recover);
  };
}

export function AppleBillingRuntime({
  lifecycle,
  userId,
  onAccountChange,
  observe = observeRecovery,
  children
}: {
  lifecycle: AppleBillingLifecycle;
  userId: string | null;
  onAccountChange: () => void;
  observe?: (lifecycle: AppleBillingLifecycle) => () => void;
  children: ReactNode;
}) {
  const state = useSyncExternalStore(
    lifecycle.subscribe,
    lifecycle.getSnapshot,
    lifecycle.getSnapshot
  );

  useLayoutEffect(() => {
    lifecycle.activate();
    const unregister = registerAppleBillingLifecycle(lifecycle);
    return () => {
      unregister();
      lifecycle.dispose();
    };
  }, [lifecycle]);

  useLayoutEffect(() => {
    onAccountChange();
    lifecycle.setUser(userId);
  }, [lifecycle, userId, onAccountChange]);

  useEffect(() => observe(lifecycle), [lifecycle, observe]);

  const actions = useMemo(() => {
    const assertOwner = () => {
      lifecycle.assertOwner(state.ownerKey);
    };
    return {
      purchase: async (productId: string) => {
        assertOwner();
        return lifecycle.purchase(productId);
      },
      restore: async () => {
        assertOwner();
        return lifecycle.restore();
      },
      retry: async () => {
        assertOwner();
        return lifecycle.retry();
      }
    };
  }, [state.ownerKey, lifecycle]);
  const value = useMemo(() => ({ ...state, ...actions, available: true }), [state, actions]);
  return <AppleBillingContext.Provider value={value}>{children}</AppleBillingContext.Provider>;
}

export function AppleBillingProvider({ children }: { children: ReactNode }) {
  return isIOS() ? <IOSAppleBillingProvider>{children}</IOSAppleBillingProvider> : <>{children}</>;
}
