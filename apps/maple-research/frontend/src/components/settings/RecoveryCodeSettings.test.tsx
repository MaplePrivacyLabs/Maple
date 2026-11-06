import { afterEach, describe, expect, mock, test } from "bun:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { RecoveryCodeSettings } from "./RecoveryCodeSettings";

const code = "MPLRC1-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-39YZ-VTQZ";
const disabled = { enrolled: false, issued_at: null, last_used_at: null, use_count: 0 };
function api(enrolled = false) {
  return {
    getRecoveryCodeStatus: mock(async () => ({ ...disabled, enrolled })),
    enrollRecoveryCode: mock(async () => ({ recovery_code: code })),
    rotateRecoveryCode: mock(async () => ({ recovery_code: code })),
    disableRecoveryCode: mock(async () => {})
  };
}
function text(node: ReactTestInstance): string {
  return node.children.map((child) => (typeof child === "string" ? child : text(child))).join("");
}
let view: ReactTestRenderer;
afterEach(() => {
  if (view) act(() => view.unmount());
});
function button(label: string) {
  return view.root.findAllByType("button").find((node) => text(node) === label)!;
}
async function click(label: string) {
  await act(async () => {
    await button(label).props.onClick();
  });
}
async function enterPassword() {
  await act(async () => {
    view.root
      .findByProps({ id: "recovery-password" })
      .props.onChange({ target: { value: "password-fixture" } });
  });
}
async function submit() {
  await act(async () => {
    await view.root.findByType("form").props.onSubmit({ preventDefault() {} });
  });
}

describe("RecoveryCodeSettings", () => {
  test("waits for a confirmed file save and keeps the code until explicitly acknowledged", async () => {
    let resolve!: (result: "saved") => void;
    const saveCode = mock(
      () =>
        new Promise<"saved">((done) => {
          resolve = done;
        })
    );
    await act(async () => {
      view = create(
        <RecoveryCodeSettings api={api()} onLockChange={() => {}} saveCode={saveCode} />
      );
    });
    await click("Enable recovery");
    await enterPassword();
    await submit();
    let pending!: Promise<void>;
    await act(async () => {
      pending = button("Save text file").props.onClick();
    });
    expect(saveCode).toHaveBeenCalledWith(code);
    expect(button("Saving…").props.disabled).toBe(true);
    expect(text(view.root)).not.toContain("file saved");
    await act(async () => {
      view.root
        .findByProps({ id: "recovery-confirmation" })
        .props.onChange({ target: { value: code } });
    });
    expect(button("Done").props.disabled).toBe(true);
    await act(async () => {
      resolve("saved");
      await pending;
    });
    expect(text(view.root)).toContain("Recovery code file saved");
    expect(view.root.findByProps({ id: "recovery-code" }).props.value).toBe(code);
    expect(button("Done").props.disabled).toBe(false);
  });

  test("cancel and failed saves retain the code and allow another attempt", async () => {
    const saveCode = mock(async (): Promise<"cancelled" | "saved"> => "cancelled");
    await act(async () => {
      view = create(
        <RecoveryCodeSettings api={api()} onLockChange={() => {}} saveCode={saveCode} />
      );
    });
    await click("Enable recovery");
    await enterPassword();
    await submit();
    await click("Save text file");
    expect(text(view.root)).not.toContain("file saved");
    expect(text(view.root)).not.toContain("Couldn't save a file");
    expect(button("Done").props.disabled).toBe(true);
    saveCode.mockImplementation(async () => {
      throw new Error("private native detail");
    });
    await click("Save text file");
    expect(text(view.root)).toContain("Couldn't save a file");
    expect(text(view.root)).not.toContain("private native detail");
    expect(view.root.findByProps({ id: "recovery-code" }).props.value).toBe(code);
    saveCode.mockImplementation(async () => "saved");
    await click("Save text file");
    expect(text(view.root)).toContain("Recovery code file saved");
    expect(button("Done").props.disabled).toBe(true);
  });

  test("enrolls with password step-up and locks until the code is saved, then forgets it", async () => {
    const service = api();
    const lock = mock<(locked: boolean) => void>(() => {});
    await act(async () => {
      view = create(<RecoveryCodeSettings api={service} onLockChange={lock} />);
    });
    await click("Enable recovery");
    await enterPassword();
    await submit();
    expect(service.enrollRecoveryCode).toHaveBeenCalledWith("password-fixture");
    expect(view.root.findByProps({ id: "recovery-code" }).props.value).toBe(code);
    expect(view.root.findAllByProps({ id: "recovery-password" })).toHaveLength(0);
    expect(lock.mock.calls.at(-1)).toEqual([true]);
    expect(button("Done").props.disabled).toBe(true);
    await act(async () => {
      view.root
        .findByProps({ id: "recovery-confirmation" })
        .props.onChange({ target: { value: code } });
    });
    await click("Done");
    expect(view.root.findAllByProps({ id: "recovery-code" })).toHaveLength(0);
    expect(lock.mock.calls.at(-1)).toEqual([false]);
  });

  test("warns before rotation and disablement, and cancellation makes no mutation", async () => {
    const service = api(true);
    await act(async () => {
      view = create(<RecoveryCodeSettings api={service} onLockChange={() => {}} />);
    });
    await click("Replace recovery code");
    expect(text(view.root)).toContain("stop working immediately");
    await click("Cancel");
    expect(service.rotateRecoveryCode).not.toHaveBeenCalled();
    await click("Disable recovery");
    expect(text(view.root)).toContain("Future password resets will delete your encrypted data");
    await enterPassword();
    await submit();
    expect(service.disableRecoveryCode).toHaveBeenCalledWith("password-fixture");
    expect(text(view.root)).toContain("Recovery is not enabled");
  });

  test("rotation displays the replacement returned by the SDK", async () => {
    const service = api(true);
    await act(async () => {
      view = create(<RecoveryCodeSettings api={service} onLockChange={() => {}} />);
    });
    await click("Replace recovery code");
    await enterPassword();
    await submit();
    expect(service.rotateRecoveryCode).toHaveBeenCalledWith("password-fixture");
    expect(text(view.root)).toContain("previous one no longer works");
    expect(view.root.findByProps({ id: "recovery-code" }).props.value).toBe(code);
  });

  test("an ambiguous mutation failure requires a status refresh and never retries automatically", async () => {
    const service = {
      ...api(),
      enrollRecoveryCode: mock(async () => {
        throw new Error("network");
      })
    };
    await act(async () => {
      view = create(<RecoveryCodeSettings api={service} onLockChange={() => {}} />);
    });
    await click("Enable recovery");
    await enterPassword();
    await submit();
    expect(text(view.root)).toContain("didn't receive a code, replace it");
    expect(view.root.findAllByType("form")).toHaveLength(0);
    service.getRecoveryCodeStatus.mockImplementation(async () => ({ ...disabled, enrolled: true }));
    await click("Refresh recovery status");
    expect(button("Replace recovery code")).toBeDefined();
    expect(service.enrollRecoveryCode).toHaveBeenCalledTimes(1);
  });

  test("password rejection clears the entered password and permits another step-up", async () => {
    const service = {
      ...api(),
      enrollRecoveryCode: mock(async () => {
        throw { status: 401 };
      })
    };
    await act(async () => {
      view = create(<RecoveryCodeSettings api={service} onLockChange={() => {}} />);
    });
    await click("Enable recovery");
    await enterPassword();
    await submit();
    expect(view.root.findByProps({ id: "recovery-password" }).props.value).toBe("");
    expect(text(view.root)).toContain("Unable to confirm your password");
  });

  test("account remount discards a late code and releases the old navigation lock", async () => {
    let resolve!: (result: { recovery_code: string }) => void;
    const service = {
      ...api(),
      enrollRecoveryCode: mock(
        () =>
          new Promise<{ recovery_code: string }>((done) => {
            resolve = done;
          })
      )
    };
    const lock = mock<(locked: boolean) => void>(() => {});
    await act(async () => {
      view = create(<RecoveryCodeSettings key="first" api={service} onLockChange={lock} />);
    });
    await click("Enable recovery");
    await enterPassword();
    let pending!: Promise<void>;
    await act(async () => {
      pending = view.root.findByType("form").props.onSubmit({ preventDefault() {} });
    });
    await act(async () => {
      view.update(<RecoveryCodeSettings key="second" api={api()} onLockChange={lock} />);
    });
    await act(async () => {
      resolve({ recovery_code: code });
      await pending;
    });
    expect(view.root.findAllByProps({ id: "recovery-code" })).toHaveLength(0);
    expect(text(view.root)).not.toContain(code);
    expect(lock.mock.calls.at(-1)).toEqual([false]);
  });
});

test("saved-code confirmation rejects typos and a different valid code without forgetting the issued code", async () => {
  const lock = mock<(locked: boolean) => void>(() => {});
  await act(async () => {
    view = create(<RecoveryCodeSettings api={api()} onLockChange={lock} />);
  });
  await click("Enable recovery");
  await enterPassword();
  await submit();
  for (const pasted of [
    code.slice(0, -1) + "0",
    "MPLRC1-000G-40R4-0M30-E209-185G-R38E-1W81-24GK-2GAH-C5RR-34D1-P70X-3RFG-948Q-DC7M"
  ]) {
    await act(async () => {
      view.root
        .findByProps({ id: "recovery-confirmation" })
        .props.onChange({ target: { value: pasted } });
    });
    await click("Done");
    expect(text(view.root)).toContain("That code doesn't match");
    expect(view.root.findByProps({ id: "recovery-code" }).props.value).toBe(code);
    expect(view.root.findByProps({ id: "recovery-confirmation" }).props.value).toBe(pasted);
    expect(lock.mock.calls.at(-1)).toEqual([true]);
  }
  await act(async () => {
    view.root.findByProps({ id: "recovery-confirmation" }).props.onChange({
      target: {
        value: `Maple recovery code\n\n${code.toLowerCase().replace(/0/g, "o")}\n\nKeep this code private.`
      }
    });
  });
  await click("Show saved code");
  expect(view.root.findByProps({ id: "recovery-confirmation" }).props.type).toBe("text");
  await click("Hide saved code");
  expect(view.root.findByProps({ id: "recovery-confirmation" }).props.type).toBe("password");
  await click("Done");
  expect(view.root.findAllByProps({ id: "recovery-code" })).toHaveLength(0);
  expect(lock.mock.calls.at(-1)).toEqual([false]);
});

test("mobile apps offer copy and confirmation without the unsupported file-save button", async () => {
  await act(async () => {
    view = create(<RecoveryCodeSettings api={api()} onLockChange={() => {}} mobile />);
  });
  await click("Enable recovery");
  await enterPassword();
  await submit();
  expect(button("Copy code")).toBeDefined();
  expect(view.root.findAllByType("button").some((node) => text(node) === "Save text file")).toBe(
    false
  );
  expect(view.root.findByProps({ id: "recovery-confirmation" })).toBeDefined();
  expect(text(view.root)).toContain("access to your account email");
});
