import { describe, expect, mock, test } from "bun:test";
import { useCallback, useEffect, useLayoutEffect, useState, type ReactNode } from "react";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { useChatRuntimeStore } from "@/contexts/ChatRuntimeContext";
import { createConversationChatKey } from "@/services/chatRuntimeStore";
import { RootRuntimeLayout } from "./RootRuntimeLayout";

function OAuthCallbackProbe({ processCallback }: { processCallback: () => void }) {
  useEffect(() => {
    processCallback();
  }, [processCallback]);

  return null;
}

function SuccessfulOAuthTransition({
  processCallback,
  accountScopedUi
}: {
  processCallback: () => void;
  accountScopedUi: ReactNode;
}) {
  const [userId, setUserId] = useState<string | null>(null);
  const completeCallback = useCallback(() => {
    processCallback();
    setUserId("authenticated-user");
  }, [processCallback]);

  return (
    <RootRuntimeLayout
      userId={userId}
      pathname="/auth/github/callback"
      authenticatedHome={null}
      routeContent={<OAuthCallbackProbe processCallback={completeCallback} />}
      accountScopedUi={accountScopedUi}
    />
  );
}

function LifecycleProbe({ onMount, onUnmount }: { onMount: () => void; onUnmount: () => void }) {
  useEffect(() => {
    onMount();
    return onUnmount;
  }, [onMount, onUnmount]);

  return null;
}

function ChatStoreProbe({ onStore }: { onStore: (store: unknown) => void }) {
  const store = useChatRuntimeStore<unknown, unknown>();

  useEffect(() => {
    onStore(store);
  }, [onStore, store]);

  return null;
}

type AccountRunBoundary = {
  previous?: {
    store: {
      getActiveRunKeys: () => readonly string[];
    };
    signal: AbortSignal;
  };
  observed?: {
    activeRunKeys: readonly string[];
    signalAborted: boolean;
  };
};

function AccountRunBoundaryProbe({
  userId,
  boundary
}: {
  userId: string;
  boundary: AccountRunBoundary;
}) {
  const store = useChatRuntimeStore<unknown, unknown>();

  useLayoutEffect(() => {
    if (userId === "user-a") {
      const runtimeKey = createConversationChatKey("account-boundary");
      store.ensure(runtimeKey);
      const run = store.beginRun(runtimeKey);
      boundary.previous = { store, signal: run.signal };
      return;
    }

    const previous = boundary.previous;
    if (!previous) return;
    boundary.observed = {
      activeRunKeys: [...previous.store.getActiveRunKeys()],
      signalAborted: previous.signal.aborted
    };
  }, [boundary, store, userId]);

  return null;
}

describe("RootRuntimeLayout", () => {
  test("keeps the OAuth callback route mounted when success authenticates a user", () => {
    const processCallback = mock(() => {});
    const globalMounted = mock(() => {});
    const globalUnmounted = mock(() => {});
    let renderer: ReactTestRenderer;

    act(() => {
      renderer = create(
        <SuccessfulOAuthTransition
          processCallback={processCallback}
          accountScopedUi={<LifecycleProbe onMount={globalMounted} onUnmount={globalUnmounted} />}
        />
      );
    });

    expect(processCallback).toHaveBeenCalledTimes(1);
    expect(globalMounted).toHaveBeenCalledTimes(2);
    expect(globalUnmounted).toHaveBeenCalledTimes(1);

    act(() => renderer.unmount());
  });

  test("keeps signup mounted while anonymous account creation authenticates the new user", () => {
    const routeMounted = mock(() => {});
    const routeUnmounted = mock(() => {});
    const signup = <LifecycleProbe onMount={routeMounted} onUnmount={routeUnmounted} />;
    let renderer: ReactTestRenderer;

    act(() => {
      renderer = create(
        <RootRuntimeLayout
          userId={null}
          pathname="/signup"
          authenticatedHome={null}
          routeContent={signup}
          accountScopedUi={null}
        />
      );
    });

    act(() => {
      renderer.update(
        <RootRuntimeLayout
          userId="new-anonymous-user"
          pathname="/signup"
          authenticatedHome={null}
          routeContent={signup}
          accountScopedUi={null}
        />
      );
    });

    expect(routeMounted).toHaveBeenCalledTimes(1);
    expect(routeUnmounted).not.toHaveBeenCalled();

    act(() => renderer.unmount());
  });

  test("remounts ordinary routes and account-scoped UI while resetting chat between users", () => {
    const stores: unknown[] = [];
    const recordStore = mock((store: unknown) => stores.push(store));
    const routeMounted = mock(() => {});
    const routeUnmounted = mock(() => {});
    const globalMounted = mock(() => {});
    const globalUnmounted = mock(() => {});
    const chat = <ChatStoreProbe onStore={recordStore} />;
    const route = <LifecycleProbe onMount={routeMounted} onUnmount={routeUnmounted} />;
    const accountScopedUi = <LifecycleProbe onMount={globalMounted} onUnmount={globalUnmounted} />;
    let renderer: ReactTestRenderer;

    act(() => {
      renderer = create(
        <RootRuntimeLayout
          userId="user-a"
          pathname="/settings/account"
          authenticatedHome={chat}
          routeContent={route}
          accountScopedUi={accountScopedUi}
        />
      );
    });

    act(() => {
      renderer.update(
        <RootRuntimeLayout
          userId="user-b"
          pathname="/settings/account"
          authenticatedHome={chat}
          routeContent={route}
          accountScopedUi={accountScopedUi}
        />
      );
    });

    expect(recordStore).toHaveBeenCalledTimes(2);
    expect(stores[1]).not.toBe(stores[0]);
    expect(routeMounted).toHaveBeenCalledTimes(2);
    expect(routeUnmounted).toHaveBeenCalledTimes(1);
    expect(globalMounted).toHaveBeenCalledTimes(2);
    expect(globalUnmounted).toHaveBeenCalledTimes(1);

    act(() => renderer.unmount());
  });

  test("cancels the previous account's queue runner before new-account layout effects", () => {
    const boundary: AccountRunBoundary = {};
    let renderer: ReactTestRenderer;

    act(() => {
      renderer = create(
        <RootRuntimeLayout
          userId="user-a"
          pathname="/"
          authenticatedHome={null}
          routeContent={<div />}
          accountScopedUi={<AccountRunBoundaryProbe userId="user-a" boundary={boundary} />}
        />
      );
    });

    expect(boundary.previous?.signal.aborted).toBe(false);

    act(() => {
      renderer.update(
        <RootRuntimeLayout
          userId="user-b"
          pathname="/"
          authenticatedHome={null}
          routeContent={<div />}
          accountScopedUi={<AccountRunBoundaryProbe userId="user-b" boundary={boundary} />}
        />
      );
    });

    expect(boundary.observed).toEqual({ activeRunKeys: [], signalAborted: true });

    act(() => renderer.unmount());
  });

  test("shares the same-account chat store when moving from home to an ordinary route", () => {
    const homeStores: unknown[] = [];
    const routeStores: unknown[] = [];
    let renderer: ReactTestRenderer;

    act(() => {
      renderer = create(
        <RootRuntimeLayout
          userId="user-a"
          pathname="/"
          authenticatedHome={<ChatStoreProbe onStore={(store) => homeStores.push(store)} />}
          routeContent={<div />}
          accountScopedUi={null}
        />
      );
    });

    act(() => {
      renderer.update(
        <RootRuntimeLayout
          userId="user-a"
          pathname="/agent"
          authenticatedHome={null}
          routeContent={<ChatStoreProbe onStore={(store) => routeStores.push(store)} />}
          accountScopedUi={null}
        />
      );
    });

    expect(homeStores).toHaveLength(1);
    expect(routeStores).toHaveLength(1);
    expect(routeStores[0]).toBe(homeStores[0]);

    act(() => renderer.unmount());
  });

  test("shares the account chat store with account-scoped UI", () => {
    const homeStores: unknown[] = [];
    const accountUiStores: unknown[] = [];
    let renderer: ReactTestRenderer;

    act(() => {
      renderer = create(
        <RootRuntimeLayout
          userId="user-a"
          pathname="/"
          authenticatedHome={<ChatStoreProbe onStore={(store) => homeStores.push(store)} />}
          routeContent={<div />}
          accountScopedUi={<ChatStoreProbe onStore={(store) => accountUiStores.push(store)} />}
        />
      );
    });

    expect(homeStores).toHaveLength(1);
    expect(accountUiStores).toHaveLength(1);
    expect(accountUiStores[0]).toBe(homeStores[0]);

    act(() => renderer.unmount());
  });

  test("retains the same-account chat store while authenticated home is temporarily hidden", () => {
    const stores: unknown[] = [];
    const recordStore = mock((store: unknown) => stores.push(store));
    const chat = <ChatStoreProbe onStore={recordStore} />;
    let renderer: ReactTestRenderer;

    act(() => {
      renderer = create(
        <RootRuntimeLayout
          userId="user-a"
          pathname="/"
          authenticatedHome={chat}
          routeContent={<div />}
          accountScopedUi={null}
        />
      );
    });

    act(() => {
      renderer.update(
        <RootRuntimeLayout
          userId="user-a"
          pathname="/settings/account"
          authenticatedHome={null}
          routeContent={<div />}
          accountScopedUi={null}
        />
      );
    });

    act(() => {
      renderer.update(
        <RootRuntimeLayout
          userId="user-a"
          pathname="/"
          authenticatedHome={chat}
          routeContent={<div />}
          accountScopedUi={null}
        />
      );
    });

    expect(recordStore).toHaveBeenCalledTimes(2);
    expect(stores[1]).toBe(stores[0]);

    act(() => renderer.unmount());
  });
});
