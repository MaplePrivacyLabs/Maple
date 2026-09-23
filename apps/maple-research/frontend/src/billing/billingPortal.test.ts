import { describe, expect, test } from "bun:test";
import { BillingSessionChangedError } from "./billingService";
import { openBillingPortal, type BillingPortalDependencies } from "./billingPortal";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((accept, fail) => {
    resolve = accept;
    reject = fail;
  });
  return { promise, resolve, reject };
}

function fixture(native = false) {
  let current = true;
  const opened: string[] = [];
  const assertCurrent = () => {
    if (!current) throw new BillingSessionChangedError();
  };
  const dependencies: BillingPortalDependencies = {
    billing: {
      captureSessionGuard: () => assertCurrent,
      getPortalUrl: async () => "https://billing.stripe.com/p/session/fixture-portal-canary"
    },
    native,
    loadOpener: async () => ({
      openUrl: async (url) => {
        opened.push(`native:${url}`);
      }
    }),
    openWindow: (url, target, features) => {
      opened.push(`web:${url}:${target}:${features}`);
      return null;
    }
  };
  return { dependencies, opened, revoke: () => (current = false) };
}

describe("billing portal ownership", () => {
  test("opens the current portal once with browser isolation even when window.open returns null", async () => {
    const f = fixture();
    await openBillingPortal(f.dependencies);
    expect(f.opened).toEqual([
      "web:https://billing.stripe.com/p/session/fixture-portal-canary:_blank:noopener,noreferrer"
    ]);
  });

  test("opens the current portal through the native opener", async () => {
    const f = fixture(true);
    await openBillingPortal(f.dependencies);
    expect(f.opened).toEqual(["native:https://billing.stripe.com/p/session/fixture-portal-canary"]);
  });

  test("a late portal response cannot open for a revoked account", async () => {
    const f = fixture();
    const url = deferred<string>();
    f.dependencies.billing.getPortalUrl = () => url.promise;
    const pending = openBillingPortal(f.dependencies);
    void pending.catch(() => {});
    f.revoke();
    url.resolve("https://billing.stripe.com/p/session/fixture-old-owner");
    await expect(pending).rejects.toBeInstanceOf(BillingSessionChangedError);
    expect(f.opened).toEqual([]);
  });

  test("revocation while loading the native module prevents both native open and browser fallback", async () => {
    const f = fixture(true);
    const started = deferred<void>();
    const loaded = deferred<Awaited<ReturnType<BillingPortalDependencies["loadOpener"]>>>();
    f.dependencies.loadOpener = () => {
      started.resolve();
      return loaded.promise;
    };
    const pending = openBillingPortal(f.dependencies);
    void pending.catch(() => {});
    await started.promise;
    f.revoke();
    loaded.resolve({ openUrl: async (url) => void f.opened.push(url) });
    await expect(pending).rejects.toBeInstanceOf(BillingSessionChangedError);
    expect(f.opened).toEqual([]);
  });

  test("a delayed native failure does not fall back after logout", async () => {
    const f = fixture(true);
    const started = deferred<void>();
    const opened = deferred<void>();
    f.dependencies.loadOpener = async () => ({
      openUrl: () => {
        started.resolve();
        return opened.promise;
      }
    });
    const pending = openBillingPortal(f.dependencies);
    void pending.catch(() => {});
    await started.promise;
    f.revoke();
    opened.reject(new Error("fixture-native-error-canary"));
    await expect(pending).rejects.toBeInstanceOf(BillingSessionChangedError);
    expect(f.opened).toEqual([]);
  });

  test("a current native failure retains a single isolated browser fallback", async () => {
    const f = fixture(true);
    f.dependencies.loadOpener = async () => ({
      openUrl: async () => {
        throw new Error("fixture-native-error-canary");
      }
    });
    await openBillingPortal(f.dependencies);
    expect(f.opened).toEqual([
      "web:https://billing.stripe.com/p/session/fixture-portal-canary:_blank:noopener,noreferrer"
    ]);
  });

  test("failures expose bounded errors without the portal URL or underlying payload", async () => {
    const f = fixture();
    f.dependencies.openWindow = () => {
      throw new Error("fixture-sensitive-url-or-opener-error-canary");
    };
    await expect(openBillingPortal(f.dependencies)).rejects.toThrow(
      "Unable to open billing portal"
    );
    f.dependencies.billing.getPortalUrl = async () => {
      throw new Error("fixture-sensitive-response-canary");
    };
    await expect(openBillingPortal(f.dependencies)).rejects.toThrow(
      "Unable to open billing portal"
    );
  });
});
