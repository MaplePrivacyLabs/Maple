import { afterEach, describe, expect, mock, test } from "bun:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { RecoveryCodeFormatError, type PasswordResetCompletion } from "@mapleai/sdk";
import { PasswordResetFlow } from "./PasswordResetFlow";
import { useState } from "react";
import { RootRuntimeLayout } from "./RootRuntimeLayout";

function text(node: ReactTestInstance): string {
  return node.children.map((child) => (typeof child === "string" ? child : text(child))).join("");
}
function service(enrolled = true, destructive = true) {
  return {
    getPasswordResetOptions: mock(async () => ({
      recovery_enrolled: enrolled,
      destructive_reset_available: destructive
    })),
    completePasswordReset: mock<
      (
        email: string,
        code: string,
        secret: string,
        password: string,
        completion: PasswordResetCompletion
      ) => Promise<void>
    >(async () => {})
  };
}
let view: ReactTestRenderer;
const RECOVERY_CODE =
  "MPLRC1-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-39YZ-VTQZ";
const props = {
  email: "reset@example.test",
  secret: "proof-fixture",
  onComplete: () => {},
  onRestart: () => {},
  onBusyChange: () => {}
};
afterEach(() => {
  if (view) act(() => view.unmount());
});
async function mount(api = service()) {
  await act(async () => {
    view = create(<PasswordResetFlow {...props} api={api} />);
  });
  return api;
}
async function input(id: string, value: string) {
  await act(async () => {
    view.root.findByProps({ id }).props.onChange({ target: { value } });
  });
}
async function submit() {
  await act(async () => {
    await view.root.findByType("form").props.onSubmit({ preventDefault() {} });
  });
}
async function verify() {
  await input("reset-email-code", "abcd1234");
  await submit();
}
async function passwords() {
  await input("reset-new-password", "new-password-fixture");
  await input("reset-confirm-password", "new-password-fixture");
}
function submitButton() {
  return view.root.findAllByType("button").find((node) => node.props.type === "submit")!;
}

describe("PasswordResetFlow", () => {
  test("verifies the email proof before exposing reset options and defaults to preserving data", async () => {
    const api = await mount();
    expect(text(view.root)).not.toContain("No recovery code is enabled");
    expect(view.root.findAllByProps({ id: "reset-new-password" })).toHaveLength(0);
    await verify();
    expect(api.getPasswordResetOptions).toHaveBeenCalledWith(props.email, "ABCD1234", props.secret);
    expect(view.root.findByProps({ value: "preserve" }).props.checked).toBe(true);
    await passwords();
    await input("reset-recovery-code", ` ${RECOVERY_CODE} `);
    await submit();
    expect(api.completePasswordReset).toHaveBeenCalledWith(
      props.email,
      "ABCD1234",
      props.secret,
      "new-password-fixture",
      { mode: "preserve", recoveryCode: RECOVERY_CODE }
    );
    expect(text(view.root)).toContain("you are signed in");
    expect(text(view.root)).not.toContain("log in with your new password");
    expect(view.root.findAllByType("input")).toHaveLength(0);
  });

  test("extracts and normalizes the code from the complete downloaded text file", async () => {
    const api = await mount();
    await verify();
    await passwords();
    const savedCode = RECOVERY_CODE.toLowerCase().replace(/0/g, "o");
    await input(
      "reset-recovery-code",
      `Maple recovery code\r\n\r\n${savedCode}\r\n\r\nKeep this code private. Replacing or disabling it makes this copy invalid.\r\n`
    );
    await submit();
    expect(api.completePasswordReset).toHaveBeenCalledWith(
      props.email,
      "ABCD1234",
      props.secret,
      "new-password-fixture",
      { mode: "preserve", recoveryCode: RECOVERY_CODE }
    );
    expect(text(view.root)).toContain("Password reset successful");
  });

  for (const pasted of ["garbage", RECOVERY_CODE.replace("39YZ", "39Yſ")]) {
    test("invalid pasted format preserves the verified options and entered text", async () => {
      const api = await mount();
      await verify();
      await passwords();
      await input("reset-recovery-code", pasted);
      await submit();
      expect(text(view.root)).toContain("Enter a valid Maple recovery code");
      expect(view.root.findByProps({ id: "reset-recovery-code" }).props.value).toBe(pasted);
      expect(view.root.findByProps({ id: "reset-new-password" }).props.value).toBe(
        "new-password-fixture"
      );
      expect(view.root.findByProps({ value: "preserve" }).props.checked).toBe(true);
      expect(api.getPasswordResetOptions).toHaveBeenCalledTimes(1);
      expect(api.completePasswordReset).not.toHaveBeenCalled();
      // Correcting the code reuses the verified proof rather than requesting it again.
      await input("reset-recovery-code", RECOVERY_CODE);
      await submit();
      expect(api.getPasswordResetOptions).toHaveBeenCalledTimes(1);
      expect(text(view.root)).toContain("Password reset successful");
    });
  }

  for (const failure of [
    new RecoveryCodeFormatError(),
    new Error("network"),
    { status: 503 },
    { status: 401 }
  ]) {
    test("SDK format and non-400 failures retain proof and inputs without a destructive fallback", async () => {
      const api = await mount();
      api.completePasswordReset.mockRejectedValue(failure);
      await verify();
      await passwords();
      await input("reset-recovery-code", RECOVERY_CODE);
      await submit();
      expect(view.root.findByProps({ id: "reset-recovery-code" }).props.value).toBe(RECOVERY_CODE);
      expect(view.root.findByProps({ id: "reset-new-password" }).props.value).toBe(
        "new-password-fixture"
      );
      expect(view.root.findByProps({ value: "preserve" }).props.checked).toBe(true);
      expect(api.completePasswordReset).toHaveBeenCalledTimes(1);
      expect(api.completePasswordReset.mock.calls[0][4].mode).toBe("preserve");
    });
  }

  test("requires explicit loss acknowledgment even when no recovery code is enrolled", async () => {
    const api = await mount(service(false));
    await verify();
    await passwords();
    expect(submitButton().props.disabled).toBe(true);
    await submit();
    expect(api.completePasswordReset).not.toHaveBeenCalled();
    await act(async () => {
      view.root.findByProps({ type: "checkbox" }).props.onChange({ target: { checked: true } });
    });
    await submit();
    expect(api.completePasswordReset).toHaveBeenCalledWith(
      props.email,
      "ABCD1234",
      props.secret,
      "new-password-fixture",
      { mode: "destructive", acknowledgeDataLoss: true }
    );
  });

  test("switching modes clears the recovery code and requires fresh consent", async () => {
    const api = await mount();
    await verify();
    await input("reset-recovery-code", "private-fixture");
    await act(async () => {
      view.root.findByProps({ value: "destructive" }).props.onChange();
    });
    expect(submitButton().props.disabled).toBe(true);
    await act(async () => {
      view.root.findByProps({ type: "checkbox" }).props.onChange({ target: { checked: true } });
    });
    await act(async () => {
      view.root.findByProps({ value: "preserve" }).props.onChange();
    });
    expect(view.root.findByProps({ id: "reset-recovery-code" }).props.value).toBe("");
    await act(async () => {
      view.root.findByProps({ value: "destructive" }).props.onChange();
    });
    expect(view.root.findByProps({ type: "checkbox" }).props.checked).toBe(false);
    expect(api.completePasswordReset).not.toHaveBeenCalled();
  });

  test("a failed proof reveals no options and makes no completion request", async () => {
    const api = service();
    api.getPasswordResetOptions.mockRejectedValue(new Error("invalid proof"));
    await mount(api);
    await verify();
    expect(text(view.root)).toContain("Couldn't verify this reset request");
    expect(view.root.findAllByProps({ id: "reset-recovery-code" })).toHaveLength(0);
    expect(api.completePasswordReset).not.toHaveBeenCalled();
  });

  test("a failed recovery reset scrubs secrets, invalidates options, and never falls back to destruction", async () => {
    const api = await mount();
    api.completePasswordReset.mockRejectedValue({ status: 400 });
    await verify();
    await passwords();
    await input("reset-recovery-code", RECOVERY_CODE);
    await submit();
    expect(text(view.root)).toContain("request a new reset email before trying again");
    expect(view.root.findAllByProps({ id: "reset-recovery-code" })).toHaveLength(0);
    expect(view.root.findAllByProps({ id: "reset-new-password" })).toHaveLength(0);
    expect(view.root.findByProps({ id: "reset-email-code" }).props.value).toBe("");
    expect(api.completePasswordReset).toHaveBeenCalledTimes(1);
    expect(api.completePasswordReset.mock.calls[0][4].mode).toBe("preserve");
  });

  test("honors unavailable destructive completion and rejects mismatched passwords locally", async () => {
    const api = await mount(service(true, false));
    await verify();
    expect(view.root.findAllByProps({ value: "destructive" })).toHaveLength(0);
    await input("reset-recovery-code", RECOVERY_CODE);
    await passwords();
    await input("reset-confirm-password", "different-password");
    await submit();
    expect(text(view.root)).toContain("make sure both passwords match");
    expect(api.completePasswordReset).not.toHaveBeenCalled();
  });

  test("ignores a late options response after a different reset request replaces the form", async () => {
    let resolve!: (options: {
      recovery_enrolled: boolean;
      destructive_reset_available: boolean;
    }) => void;
    const api = service();
    api.getPasswordResetOptions.mockImplementationOnce(
      () =>
        new Promise((done) => {
          resolve = done;
        })
    );
    await mount(api);
    await input("reset-email-code", "ABCD1234");
    let pending!: Promise<void>;
    await act(async () => {
      pending = view.root.findByType("form").props.onSubmit({ preventDefault() {} });
    });
    await act(async () => {
      view.update(<PasswordResetFlow {...props} secret="different-proof" api={api} />);
    });
    await act(async () => {
      resolve({ recovery_enrolled: true, destructive_reset_available: true });
      await pending;
    });
    expect(view.root.findAllByProps({ id: "reset-recovery-code" })).toHaveLength(0);
    expect(view.root.findByProps({ id: "reset-email-code" }).props.value).toBe("");
  });
});

for (const pathname of ["/password-reset", "/password-reset/confirm"]) {
  test(`reset completion survives the signed-in transition on ${pathname}`, async () => {
    function Transition() {
      const [userId, setUserId] = useState<string | null>(null);
      return (
        <RootRuntimeLayout
          userId={userId}
          pathname={pathname}
          authenticatedHome={null}
          accountScopedUi={null}
          routeContent={
            <PasswordResetFlow
              {...props}
              api={{
                getPasswordResetOptions: async () => ({
                  recovery_enrolled: true,
                  destructive_reset_available: true
                }),
                completePasswordReset: async () => {
                  setUserId("reset-user");
                }
              }}
            />
          }
        />
      );
    }
    await act(async () => {
      view = create(<Transition />);
    });
    await verify();
    await passwords();
    await input("reset-recovery-code", RECOVERY_CODE);
    await submit();
    expect(text(view.root)).toContain("Password reset successful");
    expect(view.root.findAllByType("input")).toHaveLength(0);
  });
}
