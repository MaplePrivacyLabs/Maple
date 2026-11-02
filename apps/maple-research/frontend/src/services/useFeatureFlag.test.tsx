import { afterEach, describe, expect, spyOn, test } from "bun:test";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { flagsClient } from "./flags";
import { useFeatureFlag } from "./useFeatureFlag";

const USER = "00000000-0000-0000-0000-000000000001";
const restores: Array<() => void> = [];

afterEach(() => {
  restores.splice(0).forEach((restore) => restore());
});

function stubFlags(isEnabled: () => Promise<boolean>) {
  const enabled = spyOn(flagsClient, "isEnabled").mockImplementation(isEnabled);
  const peek = spyOn(flagsClient, "peekIsEnabled").mockReturnValue(undefined);
  restores.push(
    () => enabled.mockRestore(),
    () => peek.mockRestore()
  );
}

async function render(userId: string | undefined) {
  const seen: Array<boolean | undefined> = [];
  function Probe() {
    seen.push(useFeatureFlag(userId, "usage_page"));
    return null;
  }
  let renderer: ReactTestRenderer | undefined;
  await act(async () => {
    renderer = create(<Probe />);
  });
  await act(async () => {});
  renderer?.unmount();
  return seen;
}

describe("useFeatureFlag", () => {
  test("is undefined while loading, then the user's value", async () => {
    stubFlags(async () => true);
    const seen = await render(USER);
    expect(seen[0]).toBeUndefined();
    expect(seen.at(-1)).toBe(true);
  });

  test("keeps optional features hidden when the flag service fails", async () => {
    const warn = spyOn(console, "warn").mockImplementation(() => {});
    restores.push(() => warn.mockRestore());
    stubFlags(async () => {
      throw new Error("offline");
    });
    expect((await render(USER)).at(-1)).toBe(false);
  });

  test("has no value without a signed-in user", async () => {
    stubFlags(async () => true);
    expect(await render(undefined)).toEqual([undefined]);
  });
});
