import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, Outlet, useLocation, useRouter } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import {
  ArrowLeft,
  BookOpen,
  Cable,
  CreditCard,
  Database,
  KeyRound,
  LogOut,
  Menu,
  MessageSquareText,
  Settings,
  ShieldCheck,
  UserRound,
  UsersRound,
  X
} from "lucide-react";
import { useEffect, useRef, useState, type ComponentType } from "react";
import { getBillingService } from "@/billing/billingService";
import { MapleWordmark } from "@/components/MapleWordmark";
import { SettingsNavigationLockProvider } from "@/components/settings/SettingsNavigationLockProvider";
import { useCompactSettingsLayout } from "@/components/settings/useCompactSettingsLayout";
import { useSettingsAuthRedirect } from "@/components/settings/useSettingsAuthRedirect";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { usePersistentHomeNavigation } from "@/contexts/PersistentHomeNavigationContext";
import {
  useSettingsNavigationLock,
  useSettingsNavigationLockState
} from "@/contexts/SettingsNavigationLockContext";
import {
  clearMapleApiAuthForUser,
  restoreMapleApiAuthForUser,
  stopAgentRuntimeForUser
} from "@/services/agentRuntimeService";
import { resetWorkspaceModePreference } from "@/services/workspaceModePreference";
import { useBillingState } from "@/state/useLocalState";
import type { TeamStatus } from "@/types/team";
import { isIOS } from "@/utils/platform";
import { getTeamSeatMismatch } from "@/utils/teamSeats";
import { cn } from "@/utils/utils";
import packageJson from "../../../package.json";
import {
  AgentConnectionsAvailabilityProvider,
  useAgentConnectionsAvailability
} from "./useAgentConnectionsAvailability";

type SettingsNavItem = {
  label: string;
  to:
    | "/settings/account"
    | "/settings/preferences"
    | "/settings/security"
    | "/settings/billing"
    | "/settings/team"
    | "/settings/api"
    | "/settings/agent-connections"
    | "/settings/history"
    | "/settings/about";
  icon: ComponentType<{ className?: string }>;
  badge?: string;
  badgeTone?: "warning" | "danger";
};

function SettingsNavLink({
  item,
  onSelect,
  replace
}: {
  item: SettingsNavItem;
  onSelect?: () => void;
  replace?: boolean;
}) {
  const Icon = item.icon;
  const isNavigationLocked = useSettingsNavigationLockState();
  const attentionLabel =
    item.badge === "Paused"
      ? "Team usage paused"
      : item.badge === "Setup"
        ? "Team setup required"
        : undefined;

  return (
    <Link
      to={item.to}
      replace={replace}
      title={item.label}
      aria-label={attentionLabel ? `${item.label}, ${attentionLabel}` : item.label}
      aria-disabled={isNavigationLocked || undefined}
      onClick={(event) => {
        if (isNavigationLocked) {
          event.preventDefault();
          return;
        }

        onSelect?.();
      }}
      activeProps={{
        className:
          "bg-[hsl(var(--sidebar-chrome))] text-foreground shadow-sm dark:bg-[hsl(var(--sidebar-chrome-hover))]"
      }}
      inactiveProps={{
        className: "text-muted-foreground hover:bg-background/70 hover:text-foreground"
      }}
      className={cn(
        "group flex min-h-11 items-center justify-start gap-3 rounded-lg px-3 py-2 text-sm font-medium transition-colors",
        isNavigationLocked && "cursor-not-allowed opacity-50"
      )}
    >
      <span className="shrink-0">
        <Icon className="h-4 w-4" />
      </span>
      <span className="min-w-0 flex-1 truncate">{item.label}</span>
      {item.badge && (
        <Badge
          variant="secondary"
          className={cn(
            "h-5 px-1.5 text-[10px]",
            item.badgeTone === "warning" &&
              "bg-maple-warning text-maple-onWarning hover:bg-maple-warning",
            item.badgeTone === "danger" &&
              "bg-destructive text-destructive-onFilled hover:bg-destructive"
          )}
        >
          {item.badge}
        </Badge>
      )}
    </Link>
  );
}

function SettingsLayoutContent() {
  const os = useOpenSecret();
  const router = useRouter();
  const location = useLocation();
  const queryClient = useQueryClient();
  const { returnToHome } = usePersistentHomeNavigation();
  const { billingStatus, setBillingStatus } = useBillingState();
  const isNavigationLocked = useSettingsNavigationLockState();
  const isCompactViewport = useCompactSettingsLayout();
  const isSettingsRoot = location.pathname === "/settings" || location.pathname === "/settings/";
  const isAuthReady = !os.auth.loading && !!os.auth.user;
  const [isDrawerOpen, setIsDrawerOpen] = useState(() => isCompactViewport && isSettingsRoot);
  const previousPathnameRef = useRef(location.pathname);
  const drawerRef = useRef<HTMLElement>(null);
  const drawerCloseButtonRef = useRef<HTMLButtonElement>(null);
  const menuButtonRef = useRef<HTMLButtonElement>(null);
  const mainRef = useRef<HTMLElement>(null);
  const [isSigningOut, setIsSigningOut] = useState(false);
  const [signOutError, setSignOutError] = useState<string | null>(null);

  useSettingsNavigationLock(isSigningOut);
  useSettingsAuthRedirect(os.auth);

  useEffect(() => {
    const pathChanged = previousPathnameRef.current !== location.pathname;
    previousPathnameRef.current = location.pathname;

    if (!isCompactViewport) {
      setIsDrawerOpen(false);
      return;
    }

    if (isSettingsRoot) {
      setIsDrawerOpen(true);
    } else if (pathChanged) {
      setIsDrawerOpen(false);
    }
  }, [isCompactViewport, isSettingsRoot, location.pathname]);

  useEffect(() => {
    if (!isAuthReady) return;

    const drawer = drawerRef.current;
    const main = mainRef.current;

    if (drawer) {
      drawer.inert = isCompactViewport && !isDrawerOpen;
    }
    if (main) {
      main.inert = isCompactViewport && isDrawerOpen;
    }

    return () => {
      if (drawer) drawer.inert = false;
      if (main) main.inert = false;
    };
  }, [isAuthReady, isCompactViewport, isDrawerOpen]);

  useEffect(() => {
    if (!isAuthReady || !isCompactViewport || !isDrawerOpen) return;

    const frame = window.requestAnimationFrame(() => drawerCloseButtonRef.current?.focus());
    return () => window.cancelAnimationFrame(frame);
  }, [isAuthReady, isCompactViewport, isDrawerOpen]);

  useEffect(() => {
    if (!isCompactViewport || !isDrawerOpen) return;

    const dismissOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      setIsDrawerOpen(false);
      window.requestAnimationFrame(() => menuButtonRef.current?.focus());
    };

    window.addEventListener("keydown", dismissOnEscape);
    return () => window.removeEventListener("keydown", dismissOnEscape);
  }, [isCompactViewport, isDrawerOpen]);

  const { data: currentBillingStatus } = useQuery({
    queryKey: ["billingStatus"],
    queryFn: async () => {
      const status = await getBillingService().getBillingStatus();
      setBillingStatus(status);
      return status;
    },
    enabled: !!os.auth.user
  });

  const resolvedBillingStatus = currentBillingStatus ?? billingStatus;
  const productName = resolvedBillingStatus?.product_name?.toLowerCase() ?? "";
  const isTeamPlan = productName.includes("team");

  const { data: teamStatus } = useQuery<TeamStatus>({
    queryKey: ["teamStatus"],
    queryFn: () => getBillingService().getTeamStatus(),
    enabled: !!os.auth.user && isTeamPlan
  });

  const isIOSPlatform = isIOS();
  const agentConnectionsAvailability = useAgentConnectionsAvailability();
  const supportsAgentConnections = agentConnectionsAvailability === "available";
  const { data: products, isError: productsError } = useQuery({
    queryKey: ["products-version-check", isIOSPlatform],
    queryFn: () => getBillingService().getProducts(`v${packageJson.version}`),
    enabled: isIOSPlatform && !!os.auth.user
  });

  if (os.auth.loading || !os.auth.user) {
    return null;
  }

  const teamSeatMismatch = getTeamSeatMismatch(teamStatus);
  const needsTeamSetup = !!teamStatus?.has_team_subscription && teamStatus.team_created === false;
  const showApiManagement =
    !isIOSPlatform ||
    productsError ||
    !!products?.some((product) => product.is_available !== false);

  const sections: Array<{ label: string; items: SettingsNavItem[] }> = [
    {
      label: "Personal",
      items: [
        { label: "Account", to: "/settings/account", icon: UserRound },
        { label: "Preferences", to: "/settings/preferences", icon: MessageSquareText },
        { label: "Security", to: "/settings/security", icon: ShieldCheck }
      ]
    },
    {
      label: "Plan",
      items: [
        { label: "Billing", to: "/settings/billing", icon: CreditCard },
        ...(isTeamPlan
          ? [
              {
                label: "Team",
                to: "/settings/team" as const,
                icon: UsersRound,
                badge: teamSeatMismatch ? "Paused" : needsTeamSetup ? "Setup" : undefined,
                badgeTone: teamSeatMismatch ? ("danger" as const) : ("warning" as const)
              }
            ]
          : [])
      ]
    },
    {
      label: "Developer",
      items: [
        ...(showApiManagement
          ? [{ label: "API & credits", to: "/settings/api" as const, icon: KeyRound }]
          : []),
        ...(supportsAgentConnections
          ? [
              {
                label: "Agent connections",
                to: "/settings/agent-connections" as const,
                icon: Cable
              }
            ]
          : [])
      ]
    },
    {
      label: "Data",
      items: [{ label: "Chat and task history", to: "/settings/history", icon: Database }]
    },
    {
      label: "Maple",
      items: [{ label: "About", to: "/settings/about", icon: BookOpen }]
    }
  ];

  const signOut = async () => {
    if (isNavigationLocked || isSigningOut) return;

    setSignOutError(null);
    setIsSigningOut(true);
    let operationBlock: Awaited<ReturnType<typeof stopAgentRuntimeForUser>> | null = null;
    let signedOut = false;
    let nativeAuthCleared = false;
    const userId = os.auth.user?.user.id;

    // Never sign out while this account may still have Agent tools executing.
    try {
      operationBlock = await stopAgentRuntimeForUser(userId);
    } catch (error) {
      console.error("Error stopping Agent Mode:", error);
      setSignOutError("Maple could not stop Agent Mode. Please try logging out again.");
      setIsSigningOut(false);
      return;
    }

    try {
      // Credential reset is required before logout so the next account cannot
      // inherit this user's local proxy key.
      const { proxyService } = await import("@/services/proxyService");
      await proxyService.stopAndResetProxy(userId, os.deleteApiKey);

      try {
        getBillingService().clearToken();
      } catch (error) {
        console.error("Error clearing billing token:", error);
        sessionStorage.removeItem("maple_billing_token");
      }

      await clearMapleApiAuthForUser(userId);
      nativeAuthCleared = true;
      await os.signOut();
      signedOut = true;
      resetWorkspaceModePreference();
      queryClient.clear();
      await router.invalidate();
      await router.navigate({ to: "/" });
    } catch (error) {
      console.error("Error during sign out:", error);
      if (signedOut) {
        window.location.href = "/";
        return;
      }
      setSignOutError(
        "Maple could not securely reset Agent Mode or finish logging out. Please try again."
      );
    } finally {
      if (!signedOut) {
        if (nativeAuthCleared) {
          try {
            await restoreMapleApiAuthForUser(userId);
          } catch (error) {
            console.error("Error restoring Maple API authentication:", error);
          }
        }
        operationBlock.release();
        setIsSigningOut(false);
      } else {
        operationBlock.retainUntilNextSession();
      }
    }
  };

  const closeSettings = () => {
    if (isNavigationLocked || isSigningOut) return;
    returnToHome();
  };

  const openDrawer = () => {
    if (isNavigationLocked || isSigningOut) return;
    setIsDrawerOpen(true);
  };

  const closeDrawer = () => {
    setIsDrawerOpen(false);
    window.requestAnimationFrame(() => menuButtonRef.current?.focus());
  };

  return (
    <div
      className="relative grid h-dvh min-h-0 w-full grid-cols-1 overflow-hidden bg-background sm:grid-cols-[16rem_minmax(0,1fr)]"
      style={isCompactViewport ? { gridTemplateColumns: "minmax(0, 1fr)" } : undefined}
    >
      <aside
        ref={drawerRef}
        aria-label="Settings navigation"
        aria-hidden={isCompactViewport && !isDrawerOpen}
        className={cn(
          "flex min-h-0 flex-col overflow-hidden border-r border-border/40 bg-muted dark:bg-[hsl(var(--sidebar))]",
          isCompactViewport
            ? "z-40 transition-transform duration-300 ease-out motion-reduce:transition-none"
            : "z-auto transition-none"
        )}
        style={
          isCompactViewport
            ? {
                position: "fixed",
                inset: 0,
                width: "100%",
                transform: isDrawerOpen ? "translateX(0)" : "translateX(-100%)"
              }
            : undefined
        }
      >
        <div className="flex h-16 shrink-0 items-center border-b border-border/30 px-3 sm:px-4">
          <button
            type="button"
            onClick={closeSettings}
            disabled={isNavigationLocked}
            className="flex h-10 min-w-0 flex-1 items-center justify-start gap-2 rounded-lg px-2 text-foreground transition-colors hover:bg-background/70 disabled:cursor-not-allowed disabled:opacity-50"
            aria-label="Back to chats"
          >
            <ArrowLeft className="h-4 w-4 shrink-0" />
            <MapleWordmark className="h-4 min-w-0 w-auto" aria-hidden />
          </button>
          {isCompactViewport && (
            <button
              ref={drawerCloseButtonRef}
              type="button"
              onClick={closeDrawer}
              className="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg text-muted-foreground transition-colors hover:bg-background/70 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              aria-label="Close settings navigation"
            >
              <X className="h-4 w-4" />
            </button>
          )}
        </div>

        <div className="flex min-h-0 flex-1 flex-col overflow-y-auto px-3 py-3">
          <div className="mb-3 flex items-center gap-2 px-3">
            <Settings className="h-4 w-4 text-muted-foreground" />
            <p className="text-sm font-semibold">Settings</p>
          </div>

          <nav className="space-y-4" aria-label="Settings categories">
            {sections
              .filter((section) => section.items.length > 0)
              .map((section) => (
                <div key={section.label}>
                  <p className="mb-1 px-3 text-[10px] font-semibold uppercase tracking-[0.12em] text-muted-foreground/80">
                    {section.label}
                  </p>
                  <div className="space-y-0.5">
                    {section.items.map((item) => (
                      <SettingsNavLink
                        key={item.to}
                        item={item}
                        onSelect={isCompactViewport ? closeDrawer : undefined}
                        replace={isCompactViewport && isSettingsRoot}
                      />
                    ))}
                  </div>
                </div>
              ))}
          </nav>
        </div>

        <div className="shrink-0 border-t border-border/30 p-3">
          <div className="mb-2 min-w-0 px-3">
            <p className="truncate text-xs font-medium">
              {os.auth.user.user.email || "Maple user"}
            </p>
            <p className="truncate text-[11px] text-muted-foreground">
              {resolvedBillingStatus?.product_name
                ? `${resolvedBillingStatus.product_name} Plan`
                : "Loading plan..."}
            </p>
          </div>
          {signOutError && (
            <p role="alert" className="mb-2 px-3 text-xs leading-relaxed text-destructive">
              {signOutError}
            </p>
          )}
          <Button
            type="button"
            variant="ghost"
            onClick={signOut}
            disabled={isNavigationLocked || isSigningOut}
            title="Log out"
            className="h-10 w-full justify-start px-3 text-muted-foreground hover:text-foreground"
          >
            <LogOut className="mr-3 h-4 w-4 shrink-0" />
            <span>{isSigningOut ? "Logging out..." : "Log out"}</span>
          </Button>
        </div>
      </aside>

      <main
        ref={mainRef}
        aria-hidden={isCompactViewport && isDrawerOpen}
        className="min-h-0 min-w-0 overflow-y-auto overscroll-y-contain bg-background"
      >
        {isCompactViewport && (
          <div className="sticky top-0 z-30 flex h-14 items-center gap-3 border-b border-border/50 bg-background/95 px-3 backdrop-blur supports-[backdrop-filter]:bg-background/80">
            <button
              ref={menuButtonRef}
              type="button"
              onClick={openDrawer}
              disabled={isNavigationLocked}
              className="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg text-foreground transition-colors hover:bg-muted disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              aria-label="Open settings navigation"
              aria-expanded={isDrawerOpen}
            >
              <Menu className="h-5 w-5" />
            </button>
            <p className="text-sm font-semibold">Settings</p>
          </div>
        )}
        <Outlet />
      </main>
    </div>
  );
}

export function SettingsLayout() {
  const os = useOpenSecret();

  return (
    <SettingsNavigationLockProvider>
      <AgentConnectionsAvailabilityProvider userId={os.auth.user?.user.id ?? null}>
        <SettingsLayoutContent />
      </AgentConnectionsAvailabilityProvider>
    </SettingsNavigationLockProvider>
  );
}
