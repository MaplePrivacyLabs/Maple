import { afterEach, describe, expect, mock, test } from "bun:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import type { PasswordResetCompletion } from "@mapleai/sdk";
import { PasswordResetFlow } from "./PasswordResetFlow";

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
    await input("reset-recovery-code", " saved-code-fixture ");
    await submit();
    expect(api.completePasswordReset).toHaveBeenCalledWith(
      props.email,
      "ABCD1234",
      props.secret,
      "new-password-fixture",
      { mode: "preserve", recoveryCode: "saved-code-fixture" }
    );
    expect(text(view.root)).toContain("you are signed in");
    expect(text(view.root)).not.toContain("log in with your new password");
    expect(view.root.findAllByType("input")).toHaveLength(0);
  });

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
    api.completePasswordReset.mockRejectedValue(new Error("rejected"));
    await verify();
    await passwords();
    await input("reset-recovery-code", "wrong-code-fixture");
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
    await input("reset-recovery-code", "saved-code-fixture");
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
