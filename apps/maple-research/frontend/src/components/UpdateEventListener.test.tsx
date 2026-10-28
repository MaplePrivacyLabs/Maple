import { afterEach, beforeEach, describe, expect, mock, spyOn, test } from "bun:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { NotificationProvider } from "@/contexts/NotificationContext";
import type { PreparedUpdate, UpdateService } from "@/services/updateService";
import { UpdateEventListener } from "./UpdateEventListener";

type PreparedHandler = (prepared: PreparedUpdate) => void;

function textContent(node: ReactTestInstance): string {
  return node.children
    .map((child) => (typeof child === "string" ? child : textContent(child)))
    .join("");
}

function service(overrides: Partial<UpdateService> = {}): UpdateService {
  return {
    loadPreferences: mock(async () => ({ automatic_updates: true })),
    savePreferences: mock(async () => {}),
    checkForUpdates: mock(async () => ({ status: "up_to_date" as const })),
    getPreparedUpdate: mock(async () => null),
    installPreparedUpdate: mock(async (version: string) => ({
      status: "ready_to_restart" as const,
      version
    })),
    restartForUpdate: mock(async () => {}),
    subscribePreparedUpdates: mock(async () => () => {}),
    ...overrides
  };
}

const listenEvent = (async () => () => {}) as typeof import("@tauri-apps/api/event").listen;

describe("UpdateEventListener", () => {
  let renderer: ReactTestRenderer | null = null;
  let consoleError: ReturnType<typeof spyOn> | null = null;

  beforeEach(() => {
    consoleError = spyOn(console, "error").mockImplementation(() => {});
  });

  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    renderer = null;
    consoleError?.mockRestore();
    consoleError = null;
  });

  async function mountListener(updates: UpdateService, eventListener = listenEvent) {
    await act(async () => {
      renderer = create(
        <NotificationProvider>
          <UpdateEventListener service={updates} isDesktop listenEvent={eventListener} />
        </NotificationProvider>
      );
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
  }

  test("does not regress a live restart-ready event with a stale install-error query", async () => {
    let preparedHandler: PreparedHandler | null = null;
    let preparedReads = 0;
    let resolveReconciliation: ((prepared: PreparedUpdate | null) => void) | null = null;
    const reconciliation = new Promise<PreparedUpdate | null>((resolve) => {
      resolveReconciliation = resolve;
    });
    const updates = service({
      subscribePreparedUpdates: mock(async (handler) => {
        preparedHandler = handler;
        return () => {};
      }),
      getPreparedUpdate: mock(async () => {
        preparedReads += 1;
        return preparedReads === 1 ? null : reconciliation;
      }),
      installPreparedUpdate: mock(async () => {
        throw new Error("concurrent install advanced");
      })
    });

    await mountListener(updates);
    act(() => {
      preparedHandler?.({
        status: "ready_to_install",
        version: "9.8.7",
        requires_system_approval: false
      });
    });
    const installButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button) === "Install Now");

    await act(async () => {
      installButton?.props.onClick();
      await Promise.resolve();
      await Promise.resolve();
    });
    act(() => {
      preparedHandler?.({ status: "ready_to_restart", version: "9.8.7" });
    });
    await act(async () => {
      resolveReconciliation?.({
        status: "ready_to_install",
        version: "9.8.7",
        requires_system_approval: false
      });
      await reconciliation;
      await Promise.resolve();
    });

    expect(textContent(renderer!.root)).toContain("Update Installed");
    expect(textContent(renderer!.root)).not.toContain("Update Not Installed");
  });

  test("shows visible recovery when an update restart fails", async () => {
    let preparedHandler: PreparedHandler | null = null;
    const updates = service({
      subscribePreparedUpdates: mock(async (handler) => {
        preparedHandler = handler;
        return () => {};
      }),
      restartForUpdate: mock(async () => {
        throw new Error("restart failed");
      })
    });

    await mountListener(updates);
    act(() => {
      preparedHandler?.({ status: "ready_to_restart", version: "9.8.7" });
    });
    const restartButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button) === "Restart Now");

    await act(async () => {
      restartButton?.props.onClick();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(textContent(renderer!.root)).toContain("Couldn't Restart Maple");
    expect(textContent(renderer!.root)).toContain("Quit and reopen Maple");
  });

  test("rehydrates prepared state when an auxiliary event listener fails", async () => {
    const updates = service({
      getPreparedUpdate: mock(async () => ({
        status: "ready_to_restart" as const,
        version: "9.8.7"
      }))
    });
    const failingListener = (async () => {
      throw new Error("event API unavailable");
    }) as typeof listenEvent;

    await mountListener(updates, failingListener);

    expect(updates.getPreparedUpdate).toHaveBeenCalledTimes(1);
    expect(textContent(renderer!.root)).toContain("Update Installed");
  });

  test("applies a newer native snapshot after an older live event", async () => {
    const updates = service({
      subscribePreparedUpdates: mock(async (handler) => {
        handler({
          status: "ready_to_install",
          version: "9.8.7",
          requires_system_approval: false
        });
        return () => {};
      }),
      getPreparedUpdate: mock(async () => ({
        status: "ready_to_restart" as const,
        version: "9.8.7"
      }))
    });

    await mountListener(updates);

    expect(textContent(renderer!.root)).toContain("Update Installed");
    expect(textContent(renderer!.root)).not.toContain("Install Now");
  });
});
