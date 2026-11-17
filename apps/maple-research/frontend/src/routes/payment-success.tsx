import { createFileRoute, Navigate } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import { getBillingService } from "@/billing/billingService";
import { isTeamPlan } from "@/billing/plan";
import { useOpenSecret } from "@mapleai/sdk";
import { Loader2 } from "lucide-react";

export const Route = createFileRoute("/payment-success")({
  component: PaymentSuccessPage
});

function PaymentSuccessPage() {
  const os = useOpenSecret();
  const isLoggedIn = !!os.auth.user;

  // Fetch billing status to check if it's a team plan
  const {
    data: billingStatus,
    isLoading,
    isError
  } = useQuery({
    queryKey: ["billingStatus", "payment-success"],
    queryFn: async () => {
      const billingService = getBillingService();
      return await billingService.getBillingStatus();
    },
    enabled: isLoggedIn,
    retry: 3, // Retry a few times in case billing status is still updating
    retryDelay: 1000 // Wait 1 second between retries
  });

  // Show loading while checking billing status
  if (isLoggedIn && isLoading) {
    return (
      <div className="flex items-center justify-center min-h-screen">
        <Loader2 className="h-8 w-8 animate-spin" />
      </div>
    );
  }

  // If there's an error fetching billing status, redirect to home
  // The billing status will be fetched again on the home page
  if (isError || !billingStatus) {
    console.error("Failed to fetch billing status after payment, redirecting to home");
    return <Navigate to="/" />;
  }

  // Check if user has a team plan
  const hasTeamPlan = isTeamPlan(billingStatus);

  // If team plan, redirect to home with team_setup param
  if (hasTeamPlan) {
    return <Navigate to="/" search={{ team_setup: true }} />;
  }

  // Otherwise, redirect to pricing page with success query parameter
  return <Navigate to="/pricing" search={{ success: true }} />;
}
