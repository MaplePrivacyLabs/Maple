import { isMobile, isTauri } from "@/utils/platform";
import {
  isAllowedZapriteCheckoutUrl,
  parseBillingApiError,
  type ZapriteUpgradeCreateResponse,
  type ZapriteUpgradeQuote,
  type ZapriteUpgradeStatusResponse
} from "./zapriteUpgrade";

// API Credit Purchase Constants
export const MIN_PURCHASE_CREDITS = 10000;
export const MIN_PURCHASE_AMOUNT = 10;
export const MIN_PURCHASE_ERROR = `Minimum purchase is ${MIN_PURCHASE_CREDITS.toLocaleString()} credits ($${MIN_PURCHASE_AMOUNT})`;

export type BillingPaymentProvider = "stripe" | "zaprite" | "subscription_pass" | "apple";

/** Management is independent of the provider selected to supply the current quota. */
export type BillingSubscription = {
  provider: string;
  plan: string;
  product_id?: string;
  state: string;
  renews_at: number | null;
  // Unknown future hints must not be treated as external URLs or executable actions.
  manage: string;
  selected?: boolean;
  environment?: string;
  auto_renew_enabled?: boolean | null;
};

export type BillingStatus = {
  is_subscribed: boolean;
  stripe_customer_id?: string | null;
  product_id: string;
  product_name: string | null;
  subscription_status: string | null;
  current_period_end: number | string | null;
  can_chat: boolean;
  chats_remaining: number | null;
  payment_provider: BillingPaymentProvider | null;
  total_tokens: number | null;
  used_tokens: number | null;
  usage_reset_date: string | null;
  api_credit_balance?: number;
  ios_iap_enabled?: boolean;
  ios_us_external_link_enabled?: boolean;
  subscriptions?: BillingSubscription[];
  conflict?: { other_provider: string; action: string } | null;
  pending_plan_change?: {
    id: string;
    type: string;
    target_plan_name: string;
    status: string;
    expires_at: string;
  } | null;
};

type BillingRecurringInfo = {
  interval: string;
  interval_count: number;
};

type BillingPriceInfo = {
  id: string;
  currency: string;
  unit_amount: number;
  recurring: BillingRecurringInfo;
};

export type BillingProduct = {
  id: string;
  name: string;
  description: string;
  active: boolean;
  default_price: BillingPriceInfo;
  is_available?: boolean;
};

// Discount Types
export type DiscountResponse =
  | {
      active: true;
      name: string;
      description: string;
      percent_off: number;
      duration_months?: number;
      starts_at: number;
      expires_at: number;
    }
  | {
      active: false;
    };

export async function fetchDiscount(): Promise<DiscountResponse> {
  try {
    const response = await fetch(
      `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/discount`,
      {
        headers: {
          "Content-Type": "application/json"
        }
      }
    );

    if (!response.ok) {
      console.error("Discount fetch error:", response.status);
      return { active: false };
    }

    return response.json() as Promise<DiscountResponse>;
  } catch (error) {
    console.error("Error fetching discount:", error);
    return { active: false };
  }
}

export async function fetchBillingStatus(thirdPartyToken: string): Promise<BillingStatus> {
  try {
    const response = await fetch(
      `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/subscription/status`,
      {
        headers: {
          Authorization: `Bearer ${thirdPartyToken}`,
          "Content-Type": "application/json"
        }
      }
    );

    if (!response.ok) {
      const errorText = await response.text();
      console.error("Billing status error response:", errorText);
      if (response.status === 401) {
        throw new Error("Unauthorized");
      }
      throw new Error(`Failed to fetch billing status: ${errorText}`);
    }

    return response.json() as Promise<BillingStatus>;
  } catch (error) {
    console.error("Error fetching billing status:", error);
    throw error;
  }
}

export async function fetchProducts(version?: string): Promise<BillingProduct[]> {
  try {
    const url = new URL(`${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/products`);
    if (version) {
      url.searchParams.append("version", version);
    }
    const response = await fetch(url.toString(), {
      headers: {
        "Content-Type": "application/json"
      }
    });

    if (!response.ok) {
      const errorText = await response.text();
      throw new Error(`Failed to fetch billing products: ${errorText}`);
    }

    return response.json() as Promise<BillingProduct[]>;
  } catch (error) {
    console.error("Error fetching billing products:", error);
    throw error;
  }
}

export async function fetchPortalUrl(thirdPartyToken: string): Promise<string> {
  let returnUrl = window.location.origin;

  // For all Tauri platforms, use the actual website origin instead of tauri://localhost
  if (isTauri()) {
    console.log("[Billing] Tauri platform detected, using trymaple.ai as return URL");
    returnUrl = "https://trymaple.ai";
  }

  const requestBody = {
    return_url: returnUrl
  };

  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/subscription/portal`,
    {
      method: "POST",
      body: JSON.stringify(requestBody),
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      }
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Portal URL error response:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    throw new Error(`Failed to fetch portal URL: ${errorText}`);
  }

  const { portal_url } = await response.json();
  return portal_url;
}

export async function createCheckoutSession(
  thirdPartyToken: string,
  email: string,
  productId: string,
  successUrl: string,
  cancelUrl: string,
  quantity?: number,
  assertCurrent?: () => void
): Promise<void> {
  const requestBody = {
    email,
    product_id: productId,
    success_url: successUrl,
    cancel_url: cancelUrl,
    ...(quantity !== undefined && { quantity })
  };
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/subscription/checkout`,
    {
      method: "POST",
      body: JSON.stringify(requestBody),
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      }
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Checkout error response:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    throw new Error(`Failed to create checkout session: ${errorText}`);
  }

  const { checkout_url } = await response.json();
  assertCurrent?.();
  console.log("Redirecting to checkout:", checkout_url);

  // For all Tauri platforms (mobile and desktop), use opener plugin to launch external browser
  if (isTauri()) {
    console.log(
      "[Billing] Tauri platform detected, using opener plugin to launch external browser"
    );

    const { invoke } = await import("@tauri-apps/api/core");
    assertCurrent?.();

    // Use the opener plugin directly - required for Tauri platforms
    await invoke("plugin:opener|open_url", { url: checkout_url })
      .then(() => {
        console.log("[Billing] Successfully opened URL in external browser");
      })
      .catch((error: Error) => {
        assertCurrent?.();
        console.error("[Billing] Failed to open external browser:", error);
        if (isMobile()) {
          throw new Error(
            "Failed to open payment page in external browser. This is required for mobile payments."
          );
        } else {
          // Fallback to window.open on desktop (less disruptive than navigating away)
          window.open(checkout_url, "_blank", "noopener");
        }
      });

    // Add a small delay to ensure the browser has time to open
    await new Promise((resolve) => setTimeout(resolve, 300));
    return;
  }

  // Fall back to regular navigation for web platforms
  assertCurrent?.();
  window.location.href = checkout_url;
}

export async function createZapriteCheckoutSession(
  thirdPartyToken: string,
  email: string,
  productId: string,
  successUrl: string,
  quantity?: number,
  assertCurrent?: () => void
): Promise<void> {
  const requestBody = {
    email,
    product_id: productId,
    success_url: successUrl,
    ...(quantity !== undefined && { quantity })
  };
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/subscription/zaprite`,
    {
      method: "POST",
      body: JSON.stringify(requestBody),
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      }
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Zaprite checkout error response:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    throw new Error(`Failed to create Zaprite checkout session: ${errorText}`);
  }

  const { checkout_url } = await response.json();
  assertCurrent?.();
  console.log("Redirecting to Zaprite checkout:", checkout_url);

  // For all Tauri platforms (mobile and desktop), use opener plugin to launch external browser
  if (isTauri()) {
    console.log(
      "[Billing] Tauri platform detected, using opener plugin to launch external browser"
    );

    const { invoke } = await import("@tauri-apps/api/core");
    assertCurrent?.();

    // Use the opener plugin directly - required for Tauri platforms
    await invoke("plugin:opener|open_url", { url: checkout_url })
      .then(() => {
        console.log("[Billing] Successfully opened URL in external browser");
      })
      .catch((error: Error) => {
        assertCurrent?.();
        console.error("[Billing] Failed to open external browser:", error);
        if (isMobile()) {
          throw new Error(
            "Failed to open payment page in external browser. This is required for mobile payments."
          );
        } else {
          // Fallback to window.open on desktop (less disruptive than navigating away)
          window.open(checkout_url, "_blank", "noopener");
        }
      });

    // Add a small delay to ensure the browser has time to open
    await new Promise((resolve) => setTimeout(resolve, 300));
    return;
  }

  // Fall back to regular navigation for web platforms
  assertCurrent?.();
  window.location.href = checkout_url;
}

async function readBillingError(response: Response, fallback: string): Promise<never> {
  const errorText = await response.text();
  if (response.status === 401) {
    throw new Error("Unauthorized");
  }
  const parsed = parseBillingApiError(errorText);
  const error = new Error(parsed.error || fallback) as Error & { code?: string };
  error.code = parsed.code;
  throw error;
}

export async function createZapriteUpgradeQuote(
  thirdPartyToken: string,
  targetProductId: string
): Promise<ZapriteUpgradeQuote> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/subscription/zaprite/upgrade-quotes`,
    {
      method: "POST",
      body: JSON.stringify({ target_product_id: targetProductId }),
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      }
    }
  );
  if (!response.ok) {
    await readBillingError(response, "Failed to create upgrade quote");
  }
  return response.json() as Promise<ZapriteUpgradeQuote>;
}

export async function createZapriteUpgrade(
  thirdPartyToken: string,
  quoteId: string,
  idempotencyKey: string,
  successUrl?: string
): Promise<ZapriteUpgradeCreateResponse> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/subscription/zaprite/upgrades`,
    {
      method: "POST",
      body: JSON.stringify({
        quote_id: quoteId,
        ...(successUrl ? { success_url: successUrl } : {})
      }),
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json",
        "Idempotency-Key": idempotencyKey
      }
    }
  );
  if (!response.ok) {
    await readBillingError(response, "Failed to create upgrade checkout");
  }
  const created = (await response.json()) as ZapriteUpgradeCreateResponse;
  if (!isAllowedZapriteCheckoutUrl(created.checkout_url)) {
    throw new Error("Checkout URL is not from the expected payment provider");
  }
  return created;
}

export async function fetchZapriteUpgradeStatus(
  thirdPartyToken: string,
  upgradeId: string
): Promise<ZapriteUpgradeStatusResponse> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/subscription/zaprite/upgrades/${upgradeId}`,
    {
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      }
    }
  );
  if (!response.ok) {
    await readBillingError(response, "Failed to fetch upgrade status");
  }
  return response.json() as Promise<ZapriteUpgradeStatusResponse>;
}

export async function openValidatedCheckoutUrl(checkoutUrl: string): Promise<void> {
  if (!isAllowedZapriteCheckoutUrl(checkoutUrl)) {
    throw new Error("Checkout URL is not from the expected payment provider");
  }
  if (isTauri()) {
    const { invoke } = await import("@tauri-apps/api/core");
    try {
      await invoke("plugin:opener|open_url", { url: checkoutUrl });
    } catch {
      if (isMobile()) {
        throw new Error(
          "Failed to open payment page in external browser. This is required for mobile payments."
        );
      }
      window.open(checkoutUrl, "_blank", "noopener");
    }
    await new Promise((resolve) => setTimeout(resolve, 300));
    return;
  }
  window.location.href = checkoutUrl;
}

// Team Management API Functions
import type {
  TeamStatus,
  CreateTeamRequest,
  CreateTeamResponse,
  InviteMembersRequest,
  InviteMembersResponse,
  TeamMembersResponse,
  CheckInviteResponse,
  AcceptInviteRequest,
  UpdateTeamNameResponse
} from "@/types/team";

export async function fetchTeamStatus(thirdPartyToken: string): Promise<TeamStatus> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/team/status`,
    {
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      }
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Team status error response:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    throw new Error(`Failed to fetch team status: ${errorText}`);
  }

  return response.json();
}

export async function createTeam(
  thirdPartyToken: string,
  data: CreateTeamRequest
): Promise<CreateTeamResponse> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/team/create`,
    {
      method: "POST",
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      },
      body: JSON.stringify(data)
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Create team error response:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    throw new Error(`Failed to create team: ${errorText}`);
  }

  return response.json();
}

export async function inviteTeamMembers(
  thirdPartyToken: string,
  data: InviteMembersRequest
): Promise<InviteMembersResponse> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/team/invites`,
    {
      method: "POST",
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      },
      body: JSON.stringify(data)
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Invite members error response:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    if (response.status === 400) {
      // Parse error message for user-friendly display
      try {
        const errorData = JSON.parse(errorText);
        throw new Error(errorData.message || errorText);
      } catch (parseError) {
        console.error("Failed to parse error response:", parseError);
        throw new Error(errorText);
      }
    }
    throw new Error(`Failed to invite members: ${errorText}`);
  }

  return response.json();
}

export async function fetchTeamMembers(thirdPartyToken: string): Promise<TeamMembersResponse> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/team/members`,
    {
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      }
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Fetch team members error response:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    throw new Error(`Failed to fetch team members: ${errorText}`);
  }

  return response.json();
}

export async function checkTeamInvite(
  thirdPartyToken: string,
  inviteId: string
): Promise<CheckInviteResponse> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/team/invites/${inviteId}/check`,
    {
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      }
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Check invite error response:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    throw new Error(`Failed to check invite: ${errorText}`);
  }

  return response.json();
}

export async function acceptTeamInvite(
  thirdPartyToken: string,
  inviteId: string,
  data: AcceptInviteRequest
): Promise<TeamStatus> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/team/invites/${inviteId}/accept`,
    {
      method: "POST",
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      },
      body: JSON.stringify(data)
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Accept invite error response:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }

    // Try to parse JSON error response for any status code
    try {
      const errorData = JSON.parse(errorText);
      throw new Error(errorData.error || errorData.message || errorText);
    } catch (parseError) {
      console.error("Failed to parse error response:", parseError);
      // If not JSON, use the text as-is
      throw new Error(errorText || "Failed to accept invitation");
    }
  }

  return response.json();
}

export async function removeTeamMember(thirdPartyToken: string, userId: string): Promise<void> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/team/members/${userId}`,
    {
      method: "DELETE",
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      }
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Remove member error response:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    if (response.status === 403) {
      throw new Error("Only team admins can remove members");
    }
    throw new Error(`Failed to remove member: ${errorText}`);
  }
}

export async function leaveTeam(thirdPartyToken: string): Promise<void> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/team/leave`,
    {
      method: "POST",
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      }
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Leave team error response:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    throw new Error(`Failed to leave team: ${errorText}`);
  }
}

export async function revokeTeamInvite(thirdPartyToken: string, inviteId: string): Promise<void> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/team/invites/${inviteId}`,
    {
      method: "DELETE",
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      }
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Revoke invite error response:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    if (response.status === 403) {
      throw new Error("Only team admins can revoke invites");
    }
    throw new Error(`Failed to revoke invite: ${errorText}`);
  }
}

export async function updateTeamName(
  thirdPartyToken: string,
  name: string
): Promise<UpdateTeamNameResponse> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/team/update`,
    {
      method: "POST",
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      },
      body: JSON.stringify({ name: name.trim() })
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Update team name error response:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    if (response.status === 400) {
      // Parse error message for user-friendly display
      try {
        const errorData = JSON.parse(errorText);
        throw new Error(errorData.message || errorText);
      } catch (parseError) {
        console.error("Failed to parse error response:", parseError);
        if (errorText.includes("team admin")) {
          throw new Error("You are not a team admin");
        }
        if (errorText.includes("between 1 and 100 characters")) {
          throw new Error("Team name must be between 1 and 100 characters");
        }
        throw new Error(errorText);
      }
    }
    throw new Error(`Failed to update team name: ${errorText}`);
  }

  return response.json();
}

// API Credits Types
export type ApiCreditBalance = {
  balance: number;
  auto_topup_enabled: boolean;
  auto_topup_threshold: number;
  auto_topup_amount: number;
};

export type ApiCreditSettings = {
  user_id: string;
  auto_topup_enabled: boolean;
  auto_topup_threshold: number;
  auto_topup_amount: number;
  stripe_customer_id: string | null;
  stripe_payment_method_id: string | null;
  created_at: string;
  updated_at: string;
};

export type PurchaseCreditsRequest = {
  credits: number;
  email: string;
  success_url: string;
  cancel_url: string;
};

export type PurchaseCreditsZapriteRequest = {
  credits: number;
  email: string;
  success_url: string;
};

export type CheckoutResponse = {
  checkout_url: string;
};

export type UpdateCreditSettingsRequest = {
  auto_topup_enabled?: boolean;
  auto_topup_threshold?: number;
  auto_topup_amount?: number;
};

// API Credits Endpoints
export async function fetchApiCreditBalance(thirdPartyToken: string): Promise<ApiCreditBalance> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/api-credits/balance`,
    {
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      }
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Fetch credit balance error:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    throw new Error(`Failed to fetch credit balance: ${errorText}`);
  }

  return response.json();
}

export async function fetchApiCreditSettings(thirdPartyToken: string): Promise<ApiCreditSettings> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/api-credits/settings`,
    {
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      }
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Fetch credit settings error:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    throw new Error(`Failed to fetch credit settings: ${errorText}`);
  }

  return response.json();
}

export async function purchaseApiCredits(
  thirdPartyToken: string,
  data: PurchaseCreditsRequest
): Promise<CheckoutResponse> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/api-credits/purchase`,
    {
      method: "POST",
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      },
      body: JSON.stringify(data)
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Purchase credits error:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    if (response.status === 400) {
      throw new Error(MIN_PURCHASE_ERROR);
    }
    throw new Error(`Failed to create checkout session: ${errorText}`);
  }

  return response.json();
}

export async function purchaseApiCreditsZaprite(
  thirdPartyToken: string,
  data: PurchaseCreditsZapriteRequest
): Promise<CheckoutResponse> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/api-credits/purchase-zaprite`,
    {
      method: "POST",
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      },
      body: JSON.stringify(data)
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Purchase credits with Zaprite error:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    if (response.status === 400) {
      throw new Error(MIN_PURCHASE_ERROR);
    }
    throw new Error(`Failed to create Zaprite checkout: ${errorText}`);
  }

  return response.json();
}

export async function updateApiCreditSettings(
  thirdPartyToken: string,
  data: UpdateCreditSettingsRequest
): Promise<ApiCreditSettings> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/api-credits/settings`,
    {
      method: "PUT",
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      },
      body: JSON.stringify(data)
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Update credit settings error:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    throw new Error(`Failed to update credit settings: ${errorText}`);
  }

  return response.json();
}

// Subscription Pass Types
export type PassStatus = "pending" | "active" | "redeemed" | "expired" | "revoked";

export type PassCheckResponse = {
  valid: boolean;
  plan_name?: string;
  plan_description?: string;
  duration_months?: number;
  status?: PassStatus;
  expires_at?: string;
  message?: string | null;
};

export type PassRedeemRequest = {
  pass_code: string;
};

export type PassRedeemResponse = {
  success: boolean;
  subscription: {
    plan_name: string;
    product_id: string;
    duration_months: number;
    start_date: string;
    end_date: string;
  };
};

// Subscription Pass Endpoints
export async function checkPassCode(passCode: string): Promise<PassCheckResponse> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/pass/check/${passCode}`,
    {
      headers: {
        "Content-Type": "application/json"
      }
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Check pass code error:", errorText);
    if (response.status === 404) {
      return {
        valid: false,
        message: "Invalid pass code"
      };
    }
    throw new Error(`Failed to check pass code: ${errorText}`);
  }

  return response.json();
}

export async function redeemPassCode(
  thirdPartyToken: string,
  data: PassRedeemRequest
): Promise<PassRedeemResponse> {
  const response = await fetch(
    `${import.meta.env.VITE_MAPLE_BILLING_API_URL}/v1/maple/pass/redeem`,
    {
      method: "POST",
      headers: {
        Authorization: `Bearer ${thirdPartyToken}`,
        "Content-Type": "application/json"
      },
      body: JSON.stringify(data)
    }
  );

  if (!response.ok) {
    const errorText = await response.text();
    console.error("Redeem pass code error:", errorText);
    if (response.status === 401) {
      throw new Error("Unauthorized");
    }
    if (response.status === 404) {
      throw new Error("Invalid pass code");
    }
    if (response.status === 400) {
      // Try to parse error message for user-friendly display
      try {
        const errorData = JSON.parse(errorText);
        throw new Error(errorData.message || errorData.error || errorText);
      } catch (parseError) {
        console.error("Failed to parse error response:", parseError);
        throw new Error(errorText);
      }
    }
    throw new Error(`Failed to redeem pass code: ${errorText}`);
  }

  return response.json();
}
