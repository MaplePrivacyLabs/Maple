import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useState,
  type Dispatch,
  type SetStateAction
} from "react";
import {
  loadSidebarOpenPreference,
  saveSidebarOpenPreference
} from "@/services/sidebarOpenPreference";

type SidebarOpenStorage = Pick<Storage, "getItem" | "setItem">;

type AccountSidebarOpenState = {
  userId: string | null;
  value: boolean | null;
};

function loadSidebarOpenState(
  userId: string | null,
  storage?: SidebarOpenStorage | null
): boolean | null {
  return userId ? loadSidebarOpenPreference(userId, storage) : null;
}

export type PersistentHomeNavigation = {
  returnToHome: (options?: { replace?: boolean }) => void;
  sidebarOpen: boolean | null;
  setSidebarOpen: Dispatch<SetStateAction<boolean | null>>;
};

export const PersistentHomeNavigationContext = createContext<PersistentHomeNavigation | null>(null);

export function usePersistentHomeNavigation() {
  const context = useContext(PersistentHomeNavigationContext);
  if (!context) {
    throw new Error(
      "usePersistentHomeNavigation must be used within PersistentHomeNavigationProvider"
    );
  }
  return context;
}

export function useAccountSidebarOpenState(
  userId: string | null,
  storage?: SidebarOpenStorage | null
): readonly [boolean | null, Dispatch<SetStateAction<boolean | null>>] {
  const [accountState, setAccountState] = useState<AccountSidebarOpenState>(() => ({
    userId,
    value: loadSidebarOpenState(userId, storage)
  }));
  const value =
    accountState.userId === userId ? accountState.value : loadSidebarOpenState(userId, storage);
  const setValue = useCallback<Dispatch<SetStateAction<boolean | null>>>(
    (nextValue) => {
      setAccountState((current) => {
        const currentValue =
          current.userId === userId ? current.value : loadSidebarOpenState(userId, storage);
        return {
          userId,
          value: typeof nextValue === "function" ? nextValue(currentValue) : nextValue
        };
      });
    },
    [storage, userId]
  );

  useLayoutEffect(() => {
    if (accountState.userId === userId) return;
    setAccountState({ userId, value: loadSidebarOpenState(userId, storage) });
  }, [accountState.userId, storage, userId]);

  useEffect(() => {
    if (!accountState.userId) return;
    const { userId: stateUserId, value: stateValue } = accountState;
    if (typeof stateValue === "boolean") {
      saveSidebarOpenPreference(stateUserId, stateValue, storage);
    }
  }, [accountState, storage]);

  return [value, setValue] as const;
}

export function usePersistentSidebarState(
  isCompactLayout: boolean
): readonly [boolean, Dispatch<SetStateAction<boolean>>] {
  const { sidebarOpen, setSidebarOpen } = usePersistentHomeNavigation();
  const isOpen = sidebarOpen ?? !isCompactLayout;
  const setIsOpen = useCallback<Dispatch<SetStateAction<boolean>>>(
    (nextValue) => {
      setSidebarOpen((currentValue) => {
        const currentIsOpen = currentValue ?? !isCompactLayout;
        return typeof nextValue === "function" ? nextValue(currentIsOpen) : nextValue;
      });
    },
    [isCompactLayout, setSidebarOpen]
  );

  return [isOpen, setIsOpen] as const;
}
