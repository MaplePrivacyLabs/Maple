import { Link, Outlet } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import {
  AlertCircle,
  CreditCard,
  KeyRound,
  Loader2,
  RotateCw,
  Server,
  Sparkles
} from "lucide-react";
import { hasApiAccess } from "@/billing/billingAccess";
import { getBillingService } from "@/billing/billingService";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { useBillingState } from "@/state/useLocalState";
import { isIOS, isTauriDesktop } from "@/utils/platform";
import { cn } from "@/utils/utils";
import { SettingsPage, SettingsSection } from "../SettingsPage";

type ApiNavLinkProps = {
  to: "/settings/api" | "/settings/api/keys" | "/settings/api/proxy";
  label: string;
  icon: typeof CreditCard;
  exact?: boolean;
};

function ApiNavLink({ to, label, icon: Icon, exact = false }: ApiNavLinkProps) {
  return (
    <Link
      to={to}
      activeOptions={{ exact }}
      activeProps={{
        className:
          "border-[hsl(var(--maple-primary))]/40 bg-[hsl(var(--maple-primary-container))] text-foreground"
      }}
      inactiveProps={{
        className: "border-border/70 text-muted-foreground hover:bg-muted/60 hover:text-foreground"
      }}
      className="flex min-h-10 min-w-0 items-center justify-center gap-2 rounded-lg border px-3 py-2 text-sm font-medium transition-colors"
    >
      <Icon className="h-4 w-4 shrink-0" />
      <span className="truncate">{label}</span>
    </Link>
  );
}

function ApiSettingsLoading() {
  return (
    <SettingsPage
      title="API & credits"
      description="Manage API access, extra credits, and local developer tools."
    >
      <SettingsSection>
        <div className="flex items-center justify-center gap-2 py-8 text-sm text-muted-foreground">
          <Loader2 className="h-5 w-5 animate-spin" />
          Loading API settings...
        </div>
      </SettingsSection>
    </SettingsPage>
  );
}

export function ApiSettingsLayout() {
  const { billingStatus, setBillingStatus } = useBillingState();
  const isIOSPlatform = isIOS();
  const isTauriDesktopPlatform = isTauriDesktop();

  const {
    data: currentBillingStatus,
    isLoading: billingStatusLoading,
    isError: billingStatusError,
    refetch: refetchBillingStatus
  } = useQuery({
    queryKey: ["billingStatus"],
    queryFn: async () => {
      const status = await getBillingService().getBillingStatus();
      setBillingStatus(status);
      return status;
    }
  });

  const resolvedBillingStatus = currentBillingStatus ?? billingStatus;

  if (billingStatusError && resolvedBillingStatus === null) {
    return (
      <SettingsPage
        title="API & credits"
        description="Manage API access, extra credits, and local developer tools."
      >
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
            <span>Unable to load your billing status.</span>
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => refetchBillingStatus()}
            >
              <RotateCw className="mr-2 h-3.5 w-3.5" />
              Try again
            </Button>
          </AlertDescription>
        </Alert>
      </SettingsPage>
    );
  }

  if (billingStatusLoading && resolvedBillingStatus === null) {
    return <ApiSettingsLoading />;
  }

  const userHasApiAccess = hasApiAccess(resolvedBillingStatus);

  if (!userHasApiAccess) {
    return (
      <SettingsPage
        title="API & credits"
        description="Manage API access, extra credits, and local developer tools."
      >
        <SettingsSection
          title="Unlock API access"
          description={
            isIOSPlatform
              ? "Upgrade to Pro or Max to create API keys and use Maple programmatically."
              : "Upgrade to Pro, Max, or a Team plan to create API keys, purchase extra credits, and use Maple programmatically."
          }
        >
          <Button asChild variant="primary">
            <Link to="/pricing">
              <Sparkles className="mr-2 h-4 w-4" />
              View pricing plans
            </Link>
          </Button>
        </SettingsSection>
      </SettingsPage>
    );
  }

  const navColumnClass = isTauriDesktopPlatform ? "grid-cols-3" : "grid-cols-2";

  return (
    <SettingsPage
      title="API & credits"
      description="Manage API keys and configure access to Maple services."
    >
      <nav aria-label="API settings" className={cn("grid gap-2", navColumnClass)}>
        <ApiNavLink to="/settings/api" label="Credits" icon={CreditCard} exact />
        <ApiNavLink to="/settings/api/keys" label="API Keys" icon={KeyRound} />
        {isTauriDesktopPlatform && (
          <ApiNavLink to="/settings/api/proxy" label="Local Proxy" icon={Server} />
        )}
      </nav>

      <Outlet />
    </SettingsPage>
  );
}
