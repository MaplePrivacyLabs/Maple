import { afterEach, describe, expect, mock, spyOn, test } from "bun:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { Switch } from "@/components/ui/switch";
import type { PreparedUpdate, UpdateService } from "@/services/updateService";
import { UpdateSettings } from "./UpdateSettings";

function textContent(node: ReactTestInstance): string {
  return node.children
    .map((child) => (typeof child === "string" ? child : textContent(child)))
    .join("");
}

function service(overrides: Partial<UpdateService> = {}): UpdateService {
  return {
    loadPreferences: mock(async () => ({ automatic_updates: false })),
    savePreferences: mock(async () => {}),
    checkForUpdates: mock(async () => ({ status: "up_to_date" as const })),
    getPreparedUpdate: mock(async () => null),
    installPreparedUpdate: mock(async (expectedVersion: string) => ({
      status: "ready_to_restart" as const,
      version: expectedVersion
    })),
    restartForUpdate: mock(async () => {}),
    subscribePreparedUpdates: mock(async () => () => {}),
    ...overrides
  };
}

describe("UpdateSettings", () => {
  let renderer: ReactTestRenderer | null = null;

  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    renderer = null;
  });

  test("loads the app preference and keeps manual checks available when it is off", async () => {
    const updates = service();

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
    });

    const toggle = renderer!.root.findByType(Switch);
    expect(toggle.props.checked).toBe(false);
    expect(toggle.props.disabled).toBe(false);
    expect(toggle.props["aria-describedby"]).toBe("automatic-updates-description");

    const checkButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button).includes("Check for updates"));
    expect(checkButton?.props.disabled).toBe(false);
  });

  test("persists a toggle before reflecting the new value", async () => {
    const updates = service();

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
    });

    await act(async () => {
      await renderer!.root.findByType(Switch).props.onCheckedChange(true);
    });

    expect(updates.savePreferences).toHaveBeenCalledWith({ automatic_updates: true });
    expect(renderer!.root.findByType(Switch).props.checked).toBe(true);
    expect(
      renderer!.root.findAll((node) => node.props.role === "status").map(textContent)
    ).toContain("Automatic updates are on. Maple will check shortly after launch and every hour.");
  });

  test("keeps the previous value and shows an actionable error when saving fails", async () => {
    const saveError = new Error("disk unavailable");
    const updates = service({
      savePreferences: mock(async () => {
        throw saveError;
      })
    });
    const consoleError = spyOn(console, "error").mockImplementation(() => {});

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
    });
    await act(async () => {
      await renderer!.root.findByType(Switch).props.onCheckedChange(true);
    });

    expect(renderer!.root.findByType(Switch).props.checked).toBe(false);
    expect(textContent(renderer!.root.find((node) => node.props.role === "alert"))).toBe(
      "Maple couldn't save your update preference. Please try again."
    );
    consoleError.mockRestore();
  });

  test("does not claim updates are off after a load failure and can explicitly persist off", async () => {
    const updates = service({
      loadPreferences: mock(async () => {
        throw new Error("invalid json");
      })
    });
    const consoleError = spyOn(console, "error").mockImplementation(() => {});

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
    });

    const toggle = renderer!.root.findByType(Switch);
    expect(toggle.props.checked).toBe(false);
    expect(toggle.props.disabled).toBe(true);
    expect(textContent(renderer!.root.find((node) => node.props.role === "alert"))).toContain(
      "Automatic update behavior is unchanged"
    );
    const checkButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button).includes("Check for updates"));
    expect(checkButton?.props.disabled).toBe(false);

    await act(async () => {
      checkButton!.props.onClick();
      await Promise.resolve();
    });

    expect(
      renderer!.root
        .findAllByType("button")
        .some((button) => textContent(button).includes("Turn automatic updates off"))
    ).toBe(true);

    const turnOffButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button).includes("Turn automatic updates off"));
    await act(async () => {
      turnOffButton!.props.onClick();
      await Promise.resolve();
    });

    expect(updates.savePreferences).toHaveBeenCalledWith({ automatic_updates: false });
    expect(renderer!.root.findByType(Switch).props.disabled).toBe(false);
    expect(renderer!.root.findByType(Switch).props.checked).toBe(false);
    consoleError.mockRestore();
  });

  test("explains that a Dev package cannot use the production updater", async () => {
    const updates = service({
      checkForUpdates: mock(async () => ({ status: "updates_disabled_for_profile" as const }))
    });
    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
    });
    const checkButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button).includes("Check for updates"));
    await act(async () => {
      checkButton!.props.onClick();
      await Promise.resolve();
    });
    expect(textContent(renderer!.root)).toContain(
      "Updates are disabled for this Maple Dev build. Install a new Dev build to update."
    );
  });

  test("turns a user-requested ready result into a persistent restart action", async () => {
    const updates = service({
      checkForUpdates: mock(async () => ({
        status: "ready_to_restart" as const,
        version: "9.8.7"
      }))
    });

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
    });
    const checkButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button).includes("Check for updates"));

    await act(async () => {
      checkButton!.props.onClick();
      await Promise.resolve();
    });

    expect(updates.checkForUpdates).toHaveBeenCalledTimes(1);
    expect(textContent(renderer!.root)).toContain(
      "Version 9.8.7 is installed. Restart Maple to apply it."
    );
    expect(
      renderer!.root
        .findAllByType("button")
        .some((button) => textContent(button).includes("Restart Maple"))
    ).toBe(true);
  });

  test("restores an install-ready action after its toast was dismissed", async () => {
    const updates = service({
      getPreparedUpdate: mock(async () => ({
        status: "ready_to_install" as const,
        version: "9.8.7",
        requires_system_approval: true
      }))
    });

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(textContent(renderer!.root)).toContain(
      "Version 9.8.7 is downloaded and signature-verified. Your system will ask for approval"
    );
    expect(
      renderer!.root
        .findAllByType("button")
        .some((button) => textContent(button).includes("Install now"))
    ).toBe(true);
  });

  test("updates a mounted Settings page when a background download becomes ready", async () => {
    let receivePreparedUpdate: ((update: PreparedUpdate) => void) | null = null;
    const updates = service({
      subscribePreparedUpdates: mock(async (handler) => {
        receivePreparedUpdate = handler;
        return () => {};
      })
    });

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
    });
    act(() => {
      receivePreparedUpdate?.({
        status: "ready_to_install",
        version: "9.8.7",
        requires_system_approval: false
      });
    });

    expect(textContent(renderer!.root)).toContain("Version 9.8.7 is downloaded");
    expect(textContent(renderer!.root)).not.toContain("Your system will ask for approval");
  });

  test("does not show a stale rehydration error after a live update arrives", async () => {
    let receivePreparedUpdate: ((update: PreparedUpdate) => void) | null = null;
    let rejectPreparedQuery: ((error: Error) => void) | null = null;
    const preparedQuery = new Promise<PreparedUpdate | null>((_resolve, reject) => {
      rejectPreparedQuery = reject;
    });
    const updates = service({
      subscribePreparedUpdates: mock(async (handler) => {
        receivePreparedUpdate = handler;
        return () => {};
      }),
      getPreparedUpdate: mock(async () => preparedQuery)
    });
    const consoleError = spyOn(console, "error").mockImplementation(() => {});

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
    });
    act(() => {
      receivePreparedUpdate?.({
        status: "ready_to_install",
        version: "9.8.7",
        requires_system_approval: false
      });
    });
    await act(async () => {
      rejectPreparedQuery?.(new Error("stale IPC failure"));
      try {
        await preparedQuery;
      } catch {
        // Expected test fixture rejection.
      }
      await Promise.resolve();
    });

    expect(textContent(renderer!.root)).toContain("Update ready to install");
    expect(textContent(renderer!.root)).not.toContain(
      "couldn't confirm whether an update is ready"
    );
    consoleError.mockRestore();
  });

  test("applies a newer native snapshot after an older live update", async () => {
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

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(textContent(renderer!.root)).toContain("Restart to finish updating");
    expect(textContent(renderer!.root)).not.toContain("Install now");
  });

  test("clears an obsolete up-to-date result when a live update becomes ready", async () => {
    let receivePreparedUpdate: ((update: PreparedUpdate) => void) | null = null;
    const updates = service({
      subscribePreparedUpdates: mock(async (handler) => {
        receivePreparedUpdate = handler;
        return () => {};
      })
    });

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
    });
    const checkButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button).includes("Check for updates"));
    await act(async () => {
      checkButton?.props.onClick();
      await Promise.resolve();
    });
    expect(textContent(renderer!.root)).toContain("Maple is up to date.");

    act(() => {
      receivePreparedUpdate?.({
        status: "ready_to_install",
        version: "9.8.7",
        requires_system_approval: false
      });
    });

    expect(textContent(renderer!.root)).toContain("Update ready to install");
    expect(textContent(renderer!.root)).not.toContain("Maple is up to date.");
  });

  test("clears an obsolete check error when a live update becomes ready", async () => {
    let receivePreparedUpdate: ((update: PreparedUpdate) => void) | null = null;
    const updates = service({
      checkForUpdates: mock(async () => {
        throw new Error("temporary network error");
      }),
      subscribePreparedUpdates: mock(async (handler) => {
        receivePreparedUpdate = handler;
        return () => {};
      })
    });
    const consoleError = spyOn(console, "error").mockImplementation(() => {});

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
    });
    const checkButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button).includes("Check for updates"));
    await act(async () => {
      checkButton?.props.onClick();
      await Promise.resolve();
    });
    expect(textContent(renderer!.root)).toContain("couldn't complete the update check");

    act(() => {
      receivePreparedUpdate?.({ status: "ready_to_restart", version: "9.8.7" });
    });

    expect(textContent(renderer!.root)).toContain("Restart to finish updating");
    expect(textContent(renderer!.root)).not.toContain("couldn't complete the update check");
    consoleError.mockRestore();
  });

  test("installs the exact prepared version and transitions to restart-ready", async () => {
    const updates = service({
      getPreparedUpdate: mock(async () => ({
        status: "ready_to_install" as const,
        version: "9.8.7",
        requires_system_approval: false
      }))
    });

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
      await Promise.resolve();
    });
    const installButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button).includes("Install now"));

    await act(async () => {
      installButton!.props.onClick();
      await Promise.resolve();
    });

    expect(updates.installPreparedUpdate).toHaveBeenCalledWith("9.8.7");
    expect(textContent(renderer!.root)).toContain("Restart to finish updating");
  });

  test("keeps a verified update available when explicit installation fails", async () => {
    const updates = service({
      getPreparedUpdate: mock(async () => ({
        status: "ready_to_install" as const,
        version: "9.8.7",
        requires_system_approval: false
      })),
      installPreparedUpdate: mock(async () => {
        throw new Error("installer cancelled");
      })
    });
    const consoleError = spyOn(console, "error").mockImplementation(() => {});

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
      await Promise.resolve();
    });
    const installButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button).includes("Install now"));

    await act(async () => {
      installButton!.props.onClick();
      await Promise.resolve();
    });

    expect(textContent(renderer!.root.find((node) => node.props.role === "alert"))).toContain(
      "verified download is still ready"
    );
    expect(textContent(renderer!.root)).toContain("Install now");
    consoleError.mockRestore();
  });

  test("reports a coalesced install that remains ready for retry", async () => {
    const updates = service({
      getPreparedUpdate: mock(async () => ({
        status: "ready_to_install" as const,
        version: "9.8.7",
        requires_system_approval: false
      })),
      installPreparedUpdate: mock(async () => ({
        status: "ready_to_install" as const,
        version: "9.8.7",
        requires_system_approval: false
      }))
    });

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
      await Promise.resolve();
    });
    const installButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button).includes("Install now"));

    await act(async () => {
      installButton?.props.onClick();
      await Promise.resolve();
    });

    expect(textContent(renderer!.root.find((node) => node.props.role === "alert"))).toContain(
      "verified download is still ready"
    );
    expect(textContent(renderer!.root)).toContain("Install now");
  });

  test("reconciles a concurrent install that already advanced to restart-ready", async () => {
    let preparedReads = 0;
    const updates = service({
      getPreparedUpdate: mock(async (): Promise<PreparedUpdate> => {
        preparedReads += 1;
        if (preparedReads === 1) {
          return {
            status: "ready_to_install",
            version: "9.8.7",
            requires_system_approval: false
          };
        }
        return { status: "ready_to_restart", version: "9.8.7" };
      }),
      installPreparedUpdate: mock(async () => {
        throw new Error("already installed by another surface");
      })
    });
    const consoleError = spyOn(console, "error").mockImplementation(() => {});

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
      await Promise.resolve();
    });
    const installButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button).includes("Install now"));

    await act(async () => {
      installButton!.props.onClick();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(textContent(renderer!.root)).toContain("Restart to finish updating");
    expect(renderer!.root.findAll((node) => node.props.role === "alert")).toHaveLength(0);
    consoleError.mockRestore();
  });

  test("does not regress a live restart-ready event with a stale install-error query", async () => {
    let receivePreparedUpdate: ((update: PreparedUpdate) => void) | null = null;
    let preparedReads = 0;
    let resolveReconciliation: ((prepared: PreparedUpdate | null) => void) | null = null;
    const reconciliation = new Promise<PreparedUpdate | null>((resolve) => {
      resolveReconciliation = resolve;
    });
    const updates = service({
      subscribePreparedUpdates: mock(async (handler) => {
        receivePreparedUpdate = handler;
        return () => {};
      }),
      getPreparedUpdate: mock(async () => {
        preparedReads += 1;
        if (preparedReads === 1) {
          return {
            status: "ready_to_install" as const,
            version: "9.8.7",
            requires_system_approval: false
          };
        }
        return reconciliation;
      }),
      installPreparedUpdate: mock(async () => {
        throw new Error("concurrent install advanced");
      })
    });
    const consoleError = spyOn(console, "error").mockImplementation(() => {});

    await act(async () => {
      renderer = create(<UpdateSettings service={updates} />);
      await Promise.resolve();
      await Promise.resolve();
    });
    const installButton = renderer!.root
      .findAllByType("button")
      .find((button) => textContent(button).includes("Install now"));

    await act(async () => {
      installButton!.props.onClick();
      await Promise.resolve();
      await Promise.resolve();
    });
    act(() => {
      receivePreparedUpdate?.({ status: "ready_to_restart", version: "9.8.7" });
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

    expect(textContent(renderer!.root)).toContain("Restart to finish updating");
    expect(renderer!.root.findAll((node) => node.props.role === "alert")).toHaveLength(0);
    consoleError.mockRestore();
  });
});
