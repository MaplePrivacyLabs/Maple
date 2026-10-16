import { afterEach, describe, expect, spyOn, test } from "bun:test";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterContextProvider
} from "@tanstack/react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import * as sdk from "@mapleai/sdk";
import * as billingService from "@/billing/billingService";
import { SettingsNavigationLockContext } from "@/contexts/SettingsNavigationLockContext";
import { ApiKeysSettings } from "./ApiKeysSettings";
import type { ApiKeyLastUsed, ApiKeySummary } from "./useApiKeys";

const USED_AT = "2026-10-01T12:00:00Z";
const API_KEYS: ApiKeySummary[] = [
  {
    name: "Used key",
    created_at: "2026-09-01T12:00:00Z",
    public_id: "0b9d6c3e-5f0a-4d43-9a39-2f7c41d1c0aa"
  },
  {
    name: "Unused key",
    created_at: "2026-08-01T12:00:00Z",
    public_id: "6142db59-fc0c-413d-8792-579fc1457fe2"
  }
];

function textContent(node: ReactTestInstance): string {
  return node.children
    .map((child) => (typeof child === "string" ? child : textContent(child)))
    .join("");
}

describe("API key last used", () => {
  let renderer: ReactTestRenderer | null = null;
  const cleanup: Array<() => void> = [];
  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    renderer = null;
    cleanup
      .splice(0)
      .reverse()
      .forEach((restore) => restore());
  });

  async function mount(lastUsed?: ApiKeyLastUsed) {
    const authSpy = spyOn(sdk, "useOpenSecret").mockReturnValue({
      auth: { user: { user: { id: "account-fixture" } }, loading: false },
      listApiKeys: async () => ({ keys: API_KEYS }),
      deleteApiKey: async () => {}
    } as unknown as ReturnType<typeof sdk.useOpenSecret>);
    const billingSpy = spyOn(billingService, "getBillingService").mockReturnValue({
      getApiKeys: () => Promise.reject(new Error("billing unavailable"))
    } as unknown as billingService.BillingService);
    cleanup.push(
      () => authSpy.mockRestore(),
      () => billingSpy.mockRestore()
    );

    const client = new QueryClient({
      defaultOptions: { queries: { retry: false, staleTime: Infinity, gcTime: 0 } }
    });
    client.setQueryData(["apiKeys"], API_KEYS);
    if (lastUsed) client.setQueryData(["apiKeyLastUsed"], lastUsed);
    cleanup.push(() => client.clear());

    const router = createRouter({
      routeTree: createRootRoute(),
      history: createMemoryHistory({ initialEntries: ["/"] }),
      isServer: false,
      origin: "http://localhost"
    });
    await router.load();

    await act(async () => {
      renderer = create(
        <RouterContextProvider router={router}>
          <QueryClientProvider client={client}>
            <SettingsNavigationLockContext.Provider
              value={{ isNavigationLocked: false, setLock: () => {} }}
            >
              <ApiKeysSettings />
            </SettingsNavigationLockContext.Provider>
          </QueryClientProvider>
        </RouterContextProvider>
      );
    });
    return textContent(renderer!.root);
  }

  test("shows each key's latest billed use from billing", async () => {
    const content = await mount(new Map([[API_KEYS[0].public_id!, USED_AT]]));
    const usedAt = new Intl.DateTimeFormat("en-US", {
      month: "short",
      day: "numeric",
      year: "numeric",
      hour: "2-digit",
      minute: "2-digit"
    }).format(new Date(USED_AT));

    expect(content).toContain(`Last used ${usedAt}`);
    expect(content).toContain("No use recorded yet");
    expect(content).toContain("most recent request that used credits");
  });

  test("leaves last used out when billing usage is unavailable", async () => {
    const content = await mount();

    expect(content).toContain("Used key");
    expect(content).not.toContain("Last used");
    expect(content).not.toContain("No use recorded yet");
  });
});
