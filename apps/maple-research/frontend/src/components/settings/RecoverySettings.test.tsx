import { afterEach, expect, mock, spyOn, test } from "bun:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { flagsClient } from "@/services/flags";
import { RecoverySettings } from "./RecoverySettings";
import { RecoveryCodeSettings } from "./RecoveryCodeSettings";

const USER = "00000000-0000-4000-8000-000000000001";
const CODE = "MPLRC1-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-39YZ-VTQZ";
let view: ReactTestRenderer;
const restores: Array<() => void> = [];
afterEach(() => {
  if (view) act(() => view.unmount());
  restores.splice(0).forEach((restore) => restore());
});
function api() {
  return {
    getRecoveryCodeStatus: mock(async () => ({
      enrolled: false,
      issued_at: null,
      last_used_at: null,
      use_count: 0
    })),
    enrollRecoveryCode: mock(async () => ({ recovery_code: CODE })),
    rotateRecoveryCode: mock(async () => ({ recovery_code: CODE })),
    disableRecoveryCode: mock(async () => {})
  };
}
function flags(load: () => Promise<boolean>, cached?: boolean) {
  const enabled = spyOn(flagsClient, "isEnabled").mockImplementation(load);
  const peek = spyOn(flagsClient, "peekIsEnabled").mockReturnValue(cached);
  restores.push(
    () => enabled.mockRestore(),
    () => peek.mockRestore()
  );
  return enabled;
}
function text(node: ReactTestInstance): string {
  return node.children.map((child) => (typeof child === "string" ? child : text(child))).join("");
}
async function click(label: string) {
  await act(async () => {
    await view.root
      .findAllByType("button")
      .find((node) => text(node) === label)!
      .props.onClick();
  });
}

test("the recovery flag controls whether management mounts or makes requests", async () => {
  const load = flags(async () => false);
  const service = api();
  await act(async () => {
    view = create(
      <RecoverySettings userId={USER} api={service} disabled={false} onLockChange={() => {}} />
    );
  });
  expect(load).toHaveBeenCalledWith(USER, "recovery");
  expect(view.root.findAllByType(RecoveryCodeSettings)).toHaveLength(0);
  expect(service.getRecoveryCodeStatus).not.toHaveBeenCalled();
  load.mockImplementation(async () => true);
  await act(async () => {
    view.update(
      <RecoverySettings
        userId={USER + "2"}
        key="new-user"
        api={service}
        disabled={false}
        onLockChange={() => {}}
      />
    );
  });
  expect(view.root.findAllByType(RecoveryCodeSettings)).toHaveLength(1);
  expect(service.getRecoveryCodeStatus).toHaveBeenCalledTimes(1);
});

test("a delayed flag-off response cannot discard an issued code before confirmation", async () => {
  let resolve!: (value: boolean) => void;
  flags(
    () =>
      new Promise<boolean>((done) => {
        resolve = done;
      }),
    true
  );
  const lock = mock((value: boolean) => {
    void value;
  });
  await act(async () => {
    view = create(
      <RecoverySettings userId={USER} api={api()} disabled={false} onLockChange={lock} />
    );
  });
  await click("Enable recovery");
  await act(async () => {
    view.root
      .findByProps({ id: "recovery-password" })
      .props.onChange({ target: { value: "password-fixture" } });
  });
  await act(async () => {
    await view.root.findByType("form").props.onSubmit({ preventDefault() {} });
  });
  await act(async () => {
    resolve(false);
  });
  expect(view.root.findByProps({ id: "recovery-code" }).props.value).toBe(CODE);
  expect(lock.mock.calls.at(-1)).toEqual([true]);
  await act(async () => {
    view.root
      .findByProps({ id: "recovery-confirmation" })
      .props.onChange({ target: { value: CODE } });
  });
  await click("Done");
  expect(view.root.findAllByType(RecoveryCodeSettings)).toHaveLength(0);
  expect(lock.mock.calls.at(-1)).toEqual([false]);
});
