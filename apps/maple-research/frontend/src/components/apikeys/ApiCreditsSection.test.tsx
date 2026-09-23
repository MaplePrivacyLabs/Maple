import { afterEach, describe, expect, spyOn, test } from "bun:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import * as sdk from "@mapleai/sdk";
import * as platform from "@/utils/platform";
import { ApiCreditsSection } from "./ApiCreditsSection";

function textContent(node: ReactTestInstance): string {
  return node.children
    .map((child) => (typeof child === "string" ? child : textContent(child)))
    .join("");
}

describe("API credits platform policy", () => {
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

  async function mount(ios: boolean) {
    const iosSpy = spyOn(platform, "isIOS").mockReturnValue(ios);
    const mobileSpy = spyOn(platform, "isMobile").mockReturnValue(ios);
    const authSpy = spyOn(sdk, "useOpenSecret").mockReturnValue({
      auth: {
        user: {
          user: { id: "account-fixture", email: "test@example.invalid", login_method: "email" }
        },
        loading: false
      }
    } as ReturnType<typeof sdk.useOpenSecret>);
    cleanup.push(
      () => iosSpy.mockRestore(),
      () => mobileSpy.mockRestore(),
      () => authSpy.mockRestore()
    );
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false, staleTime: Infinity, gcTime: 0 } }
    });
    client.setQueryData(["apiCreditBalance"], { balance: 12345 });
    cleanup.push(() => client.clear());
    await act(async () => {
      renderer = create(
        <QueryClientProvider client={client}>
          <ApiCreditsSection />
        </QueryClientProvider>
      );
    });
  }

  test("iOS retains existing credit balance and consumption explanation without checkout", async () => {
    await mount(true);
    expect(textContent(renderer!.root)).toContain("Extra Credit Balance");
    expect(textContent(renderer!.root)).toContain("12,345");
    expect(textContent(renderer!.root)).toContain(
      "Extends your subscription when plan credits run out"
    );
    expect(textContent(renderer!.root)).not.toContain("Purchase Credits");
    expect(textContent(renderer!.root)).not.toContain("$1 per 1,000 credits");
    expect(renderer!.root.findAllByType("button")).toHaveLength(0);
  });

  test("other clients preserve credit packages, custom amounts, and both payment methods", async () => {
    await mount(false);
    const content = textContent(renderer!.root);
    for (const text of [
      "12,345",
      "Purchase Credits",
      "Custom Amount",
      "Pay with Card",
      "Pay with Bitcoin",
      "$1 per 1,000 credits"
    ]) {
      expect(content).toContain(text);
    }
    expect(renderer!.root.findAllByType("button")).toHaveLength(7);
  });
});
