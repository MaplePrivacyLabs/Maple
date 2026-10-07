import { Link } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import { FullPageMain } from "@/components/FullPageMain";
import { Button } from "@/components/ui/button";

/** A plan-information destination for old upgrade links and auth callbacks. */
export function AndroidPlanPage() {
  const signedIn = !!useOpenSecret().auth.user;
  return (
    <FullPageMain className="h-dvh overflow-y-auto bg-background text-foreground">
      <section className="mx-auto flex w-full max-w-lg flex-col gap-5">
        <h1 className="text-3xl font-semibold">Your Maple plan</h1>
        <p>
          Purchases and plan changes are not available in the Android app. You can manage your
          subscription on the Maple website.
        </p>
        <p className="text-muted-foreground">
          Sign in with the same Maple account to use your existing subscription and credits here.
        </p>
        <Button asChild variant="primary">
          <Link to={signedIn ? "/settings/billing" : "/login"}>
            {signedIn ? "View current plan" : "Sign in"}
          </Link>
        </Button>
        {signedIn && (
          <Button asChild variant="outline">
            <Link to="/redeem">Redeem existing pass</Link>
          </Button>
        )}
        <Button asChild variant="outline">
          <Link to="/">Back to Maple</Link>
        </Button>
      </section>
    </FullPageMain>
  );
}
