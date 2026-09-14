import { afterEach, describe, expect, test } from "bun:test";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  RouterContextProvider
} from "@tanstack/react-router";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { useSettingsAuthRedirect } from "./useSettingsAuthRedirect";

type Auth = Parameters<typeof useSettingsAuthRedirect>[0];

function RetainedSettingsGuard({ auth }: { auth: Auth }) {
  useSettingsAuthRedirect(auth);
  return null;
}

const cleanups: Array<() => void> = [];

afterEach(() => {
  act(() => {
    for (const cleanup of cleanups.splice(0).reverse()) cleanup();
  });
});

async function mountGuard(initialHref: string, auth: Auth) {
  const rootRoute = createRootRoute({
    validateSearch: (search: Record<string, unknown>) => search
  });
  const routes = [
    "/",
    "/settings",
    "/settings/account",
    "/login",
    "/signup",
    "/settings-other"
  ].map((path) => createRoute({ getParentRoute: () => rootRoute, path }));
  const history = createMemoryHistory({ initialEntries: [initialHref] });
  const router = createRouter({
    routeTree: rootRoute.addChildren(routes),
    history,
    isServer: false,
    origin: "http://localhost"
  });
  await router.load();

  // Advance route loading explicitly below. This keeps the exiting guard
  // mounted across navigation and bounds a regressed redirect loop in the test.
  const navigations: Array<{ href: string; action: string }> = [];
  cleanups.push(
    history.subscribe(({ location, action }) => {
      navigations.push({ href: location.href, action: action.type });
    })
  );
  const renderGuard = (nextAuth: Auth) => (
    <RouterContextProvider router={router}>
      <RetainedSettingsGuard auth={nextAuth} />
    </RouterContextProvider>
  );
  let renderer!: ReactTestRenderer;
  await act(async () => {
    renderer = create(renderGuard(auth));
  });
  cleanups.push(() => renderer.unmount());

  return {
    history,
    router,
    navigations,
    async setAuth(nextAuth: Auth) {
      await act(async () => renderer.update(renderGuard(nextAuth)));
    },
    async finishNavigation() {
      await act(async () => router.load());
    }
  };
}

describe("useSettingsAuthRedirect", () => {
  test("redirects once when logout publishes signed-out state before Settings unmounts", async () => {
    const settingsHref = "/settings/account?tab=profile#details";
    const guard = await mountGuard(settingsHref, { loading: false, user: { id: "test-user" } });
    expect(guard.navigations).toEqual([]);

    await guard.setAuth({ loading: false });
    expect(guard.navigations).toHaveLength(1);
    expect(guard.navigations[0].action).toBe("REPLACE");
    const loginUrl = new URL(guard.history.location.href, "http://localhost");
    expect(loginUrl.pathname).toBe("/login");
    expect(loginUrl.searchParams.get("next")).toBe(settingsHref);

    // The old layout now sees /login but is deliberately still mounted.
    await guard.finishNavigation();
    await guard.finishNavigation();
    expect(guard.navigations).toHaveLength(1);
    expect(guard.history.location.href).toBe(guard.navigations[0].href);
  });

  for (const path of ["/settings", "/settings/account"]) {
    test(`redirects signed-out access to ${path}`, async () => {
      const guard = await mountGuard(path, { loading: false });
      expect(guard.navigations).toHaveLength(1);
      expect(
        new URL(guard.history.location.href, "http://localhost").searchParams.get("next")
      ).toBe(path);
      await guard.finishNavigation();
      expect(guard.navigations).toHaveLength(1);
    });
  }

  for (const path of ["/", "/login?next=%2Fsettings%2Faccount", "/signup", "/settings-other"]) {
    test(`does not redirect a retained Settings guard on ${path}`, async () => {
      const guard = await mountGuard(path, { loading: false });
      await guard.finishNavigation();
      expect(guard.navigations).toEqual([]);
      expect(guard.history.location.href).toBe(path);
    });
  }

  test("waits for authentication to finish loading", async () => {
    const guard = await mountGuard("/settings/account", { loading: true });
    expect(guard.navigations).toEqual([]);
    await guard.setAuth({ loading: false, user: { id: "test-user" } });
    await guard.finishNavigation();
    expect(guard.navigations).toEqual([]);
  });

  test("redirects mixed-case Settings paths when loading finishes signed out", async () => {
    const settingsHref = "/Settings/AcCoUnT?tab=ProFile#DETAILS";
    const guard = await mountGuard(settingsHref, { loading: true });
    expect(guard.router.state.matches.map((match) => match.routeId)).toContain("/settings/account");
    expect(guard.navigations).toEqual([]);

    await guard.setAuth({ loading: false });
    expect(guard.navigations).toHaveLength(1);
    expect(new URL(guard.history.location.href, "http://localhost").searchParams.get("next")).toBe(
      settingsHref
    );
    await guard.finishNavigation();
    expect(guard.navigations).toHaveLength(1);
  });
});
