import { useLocation, useRouter } from "@tanstack/react-router";
import { useEffect } from "react";

export function useSettingsAuthRedirect(auth: { loading: boolean; user?: unknown }) {
  const router = useRouter();
  const location = useLocation();

  useEffect(() => {
    // The exiting Settings layout can observe the next location before it
    // unmounts. Redirect only its own routes, never the login destination.
    // Match the router's case-insensitive paths without changing the return URL.
    const pathname = location.pathname.toLowerCase();
    const isSettingsRoute = pathname === "/settings" || pathname.startsWith("/settings/");
    if (isSettingsRoute && !auth.loading && !auth.user) {
      void router.navigate({
        to: "/login",
        search: { next: location.href },
        replace: true
      });
    }
  }, [auth.loading, auth.user, location.href, location.pathname, router]);
}
