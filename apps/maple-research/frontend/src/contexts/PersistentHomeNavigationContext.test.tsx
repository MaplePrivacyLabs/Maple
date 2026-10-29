import { afterEach, describe, expect, test } from "bun:test";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { useState, type Dispatch, type SetStateAction } from "react";

import {
  PersistentHomeNavigationContext,
  useAccountSidebarOpenState,
  usePersistentSidebarState
} from "./PersistentHomeNavigationContext";

type SidebarOpenStorage = Pick<Storage, "getItem" | "setItem">;

class MemoryStorage implements SidebarOpenStorage {
  readonly values = new Map<string, string>();

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }
}

type SidebarControls = {
  firstOpen: boolean;
  secondOpen: boolean;
  setFirstOpen: (isOpen: boolean) => void;
  setSecondOpen: (isOpen: boolean) => void;
};

let controls: SidebarControls | null = null;
let persistedControls: {
  setValue: Dispatch<SetStateAction<boolean | null>>;
  value: boolean | null;
} | null = null;

function SidebarProbe() {
  const [firstOpen, setFirstOpen] = usePersistentSidebarState(false);
  const [secondOpen, setSecondOpen] = usePersistentSidebarState(false);
  controls = { firstOpen, secondOpen, setFirstOpen, setSecondOpen };
  return <span>{`${firstOpen}:${secondOpen}`}</span>;
}

function SidebarHarness() {
  const [sidebarOpen, setSidebarOpen] = useState<boolean | null>(null);

  return (
    <PersistentHomeNavigationContext.Provider
      value={{
        returnToHome: () => {},
        setSidebarOpen,
        sidebarOpen
      }}
    >
      <SidebarProbe />
    </PersistentHomeNavigationContext.Provider>
  );
}

function PersistedSidebarProbe({
  storage,
  userId
}: {
  storage: SidebarOpenStorage;
  userId: string | null;
}) {
  const [value, setValue] = useAccountSidebarOpenState(userId, storage);
  persistedControls = { setValue, value };
  return <span>{String(value)}</span>;
}

describe("persistent sidebar state", () => {
  let renderer: ReactTestRenderer | null = null;

  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    renderer = null;
    controls = null;
    persistedControls = null;
  });

  test("shares one open state across Chat and Agent", () => {
    act(() => {
      renderer = create(<SidebarHarness />);
    });

    expect(renderer?.toJSON()).toMatchObject({ children: ["true:true"] });
    act(() => controls?.setFirstOpen(false));
    expect(renderer?.toJSON()).toMatchObject({ children: ["false:false"] });
    act(() => controls?.setSecondOpen(true));
    expect(renderer?.toJSON()).toMatchObject({ children: ["true:true"] });
  });

  test("restores closed state across remounts without crossing accounts", () => {
    const storage = new MemoryStorage();

    act(() => {
      renderer = create(<PersistedSidebarProbe storage={storage} userId="account/a" />);
    });
    act(() => persistedControls?.setValue(false));
    expect(renderer?.toJSON()).toMatchObject({ children: ["false"] });

    act(() => renderer?.unmount());
    act(() => {
      renderer = create(<PersistedSidebarProbe storage={storage} userId="account/a" />);
    });
    expect(renderer?.toJSON()).toMatchObject({ children: ["false"] });

    act(() => {
      renderer?.update(<PersistedSidebarProbe storage={storage} userId="account/b" />);
    });
    expect(renderer?.toJSON()).toMatchObject({ children: ["null"] });
    act(() => persistedControls?.setValue(true));

    act(() => {
      renderer?.update(<PersistedSidebarProbe storage={storage} userId="account/a" />);
    });
    expect(renderer?.toJSON()).toMatchObject({ children: ["false"] });

    const keysBeforeLogout = [...storage.values.keys()];
    act(() => {
      renderer?.update(<PersistedSidebarProbe storage={storage} userId={null} />);
    });
    act(() => persistedControls?.setValue(false));
    expect([...storage.values.keys()]).toEqual(keysBeforeLogout);
  });
});
