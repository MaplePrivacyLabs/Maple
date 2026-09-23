import { readNativeUserAuth, type OpenSecretContextType } from "@mapleai/sdk";
import {
  fetchBillingStatus,
  fetchPortalUrl,
  fetchProducts,
  fetchDiscount,
  createCheckoutSession,
  createZapriteCheckoutSession,
  BillingStatus,
  BillingProduct,
  DiscountResponse,
  fetchTeamStatus,
  createTeam,
  inviteTeamMembers,
  fetchTeamMembers,
  checkTeamInvite,
  acceptTeamInvite,
  removeTeamMember,
  leaveTeam,
  revokeTeamInvite,
  updateTeamName,
  fetchApiCreditBalance,
  fetchApiCreditSettings,
  purchaseApiCredits,
  purchaseApiCreditsZaprite,
  updateApiCreditSettings,
  ApiCreditBalance,
  ApiCreditSettings,
  PurchaseCreditsRequest,
  PurchaseCreditsZapriteRequest,
  CheckoutResponse,
  UpdateCreditSettingsRequest,
  checkPassCode,
  redeemPassCode,
  PassCheckResponse,
  PassRedeemRequest,
  PassRedeemResponse,
  createZapriteUpgradeQuote,
  createZapriteUpgrade,
  fetchZapriteUpgradeStatus
} from "./billingApi";
import type {
  ZapriteUpgradeCreateResponse,
  ZapriteUpgradeQuote,
  ZapriteUpgradeStatusResponse
} from "./zapriteUpgrade";
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

const TOKEN_STORAGE_KEY = "maple_billing_token";

/** Credential identity only; never retain the SDK's credentials or cache root. */
export interface BillingIdentity {
  apiOrigin: string;
  principalId: string | null;
  revision: number;
}

interface BillingOwner extends BillingIdentity {
  mintToken: OpenSecretContextType["generateThirdPartyToken"];
  token?: string;
  tokenRequest?: Promise<string>;
}

export class BillingSessionChangedError extends Error {
  constructor() {
    super("billing_session_changed");
    this.name = "BillingSessionChangedError";
  }
}

export class BillingService {
  private os: OpenSecretContextType;
  private owner: BillingOwner | undefined;
  private readonly readIdentity: () => BillingIdentity;

  constructor(os: OpenSecretContextType, readIdentity?: () => BillingIdentity) {
    this.os = os;
    this.readIdentity =
      readIdentity ??
      (() => {
        const { apiOrigin, principalId, revision } = readNativeUserAuth(this.os.apiUrl);
        return { apiOrigin, principalId, revision };
      });
    this.removeLegacyToken();
  }

  updateOpenSecret(os: OpenSecretContextType): void {
    this.os = os;
    if (this.owner) {
      try {
        this.assertOwner(this.owner);
      } catch {
        // React may update the context during logout or a credential refresh.
        // The revoked owner's pending operations fail at their next boundary.
      }
    }
  }

  private removeLegacyToken(): void {
    // Older builds persisted a token without an account or credential revision.
    // Never adopt it. Keeping the replacement in memory also prevents a reload
    // from accidentally trusting a revision from a previous SDK runtime.
    try {
      sessionStorage.removeItem(TOKEN_STORAGE_KEY);
    } catch {
      // Disabled browser storage must not prevent in-memory ownership cleanup.
    }
  }

  private identity(): BillingIdentity {
    try {
      const { apiOrigin, principalId, revision } = this.readIdentity();
      if (
        !principalId ||
        principalId !== this.os.auth.user?.user.id ||
        apiOrigin !== new URL(this.os.apiUrl).origin ||
        !Number.isSafeInteger(revision) ||
        revision < 0
      ) {
        throw new BillingSessionChangedError();
      }
      return { apiOrigin, principalId, revision };
    } catch {
      throw new BillingSessionChangedError();
    }
  }

  private matches(owner: BillingOwner, identity: BillingIdentity): boolean {
    return (
      owner.apiOrigin === identity.apiOrigin &&
      owner.principalId === identity.principalId &&
      owner.revision === identity.revision
    );
  }

  private currentOwner(): BillingOwner {
    let identity: BillingIdentity;
    try {
      identity = this.identity();
    } catch (error) {
      this.clearToken();
      throw error;
    }
    if (!this.owner || !this.matches(this.owner, identity)) {
      this.clearToken();
      this.owner = {
        ...identity,
        mintToken: this.os.generateThirdPartyToken.bind(this.os)
      };
    }
    return this.owner;
  }

  private assertOwner(owner: BillingOwner): void {
    try {
      if (this.owner === owner && this.matches(owner, this.identity())) return;
    } catch {
      // A failed authority read revokes the owner just like an account change.
    }
    if (this.owner === owner) this.clearToken();
    throw new BillingSessionChangedError();
  }

  private billingToken(owner: BillingOwner): Promise<string> {
    this.assertOwner(owner);
    if (owner.token) return Promise.resolve(owner.token);
    if (owner.tokenRequest) return owner.tokenRequest;
    const request = this.generateToken(owner);
    owner.tokenRequest = request;
    void request
      .finally(() => {
        if (owner.tokenRequest === request) owner.tokenRequest = undefined;
      })
      .catch(() => {});
    return request;
  }

  private async generateToken(owner: BillingOwner): Promise<string> {
    this.assertOwner(owner);
    let result: { token: string };
    try {
      result = await owner.mintToken(import.meta.env.VITE_MAPLE_BILLING_API_URL);
    } catch {
      this.assertOwner(owner);
      throw new Error("Billing authentication unavailable");
    }
    this.assertOwner(owner);
    if (typeof result.token !== "string" || !result.token) {
      throw new Error("Billing authentication unavailable");
    }
    owner.token = result.token;
    return result.token;
  }

  private async executeWithToken<T>(
    apiCall: (token: string, assertCurrent: () => void) => Promise<T>
  ): Promise<T> {
    const owner = this.currentOwner();
    let token = await this.billingToken(owner);
    for (let attempt = 0; attempt < 2; attempt++) {
      this.assertOwner(owner);
      try {
        const result = await apiCall(token, () => this.assertOwner(owner));
        this.assertOwner(owner);
        return result;
      } catch (error) {
        this.assertOwner(owner);
        if (
          attempt !== 0 ||
          !(error instanceof Error) ||
          !/unauthorized|Invalid JWT token|401/i.test(error.message)
        ) {
          throw error;
        }
        // A concurrent request may already have replaced this rejected token.
        // A late 401 must not clear that replacement or start another mint.
        if (owner.token === token) owner.token = undefined;
        token = await this.billingToken(owner);
      }
    }
    throw new Error("Billing authentication unavailable");
  }

  /** Fence caller-owned effects that happen after an authenticated API result. */
  captureSessionGuard(): () => void {
    const owner = this.currentOwner();
    return () => this.assertOwner(owner);
  }

  async getBillingStatus(): Promise<BillingStatus> {
    return this.executeWithToken((token) => fetchBillingStatus(token));
  }

  async getPortalUrl(): Promise<string> {
    return this.executeWithToken((token) => fetchPortalUrl(token));
  }

  async getProducts(version?: string): Promise<BillingProduct[]> {
    return fetchProducts(version);
  }

  async getDiscount(): Promise<DiscountResponse> {
    return fetchDiscount();
  }

  async createCheckoutSession(
    email: string,
    productId: string,
    successUrl: string,
    cancelUrl: string,
    quantity?: number
  ): Promise<void> {
    return this.executeWithToken((token, assertCurrent) =>
      createCheckoutSession(token, email, productId, successUrl, cancelUrl, quantity, assertCurrent)
    );
  }

  async createZapriteCheckoutSession(
    email: string,
    productId: string,
    successUrl: string,
    quantity?: number
  ): Promise<void> {
    return this.executeWithToken((token, assertCurrent) =>
      createZapriteCheckoutSession(token, email, productId, successUrl, quantity, assertCurrent)
    );
  }

  clearToken(): void {
    if (this.owner) {
      this.owner.token = undefined;
      this.owner.tokenRequest = undefined;
    }
    this.owner = undefined;
    this.removeLegacyToken();
  }

  // Team Management Methods
  async getTeamStatus(): Promise<TeamStatus> {
    return this.executeWithToken((token) => fetchTeamStatus(token));
  }

  async createTeam(data: CreateTeamRequest): Promise<CreateTeamResponse> {
    return this.executeWithToken((token) => createTeam(token, data));
  }

  async inviteTeamMembers(data: InviteMembersRequest): Promise<InviteMembersResponse> {
    return this.executeWithToken((token) => inviteTeamMembers(token, data));
  }

  async getTeamMembers(): Promise<TeamMembersResponse> {
    return this.executeWithToken((token) => fetchTeamMembers(token));
  }

  async checkTeamInvite(inviteId: string): Promise<CheckInviteResponse> {
    return this.executeWithToken((token) => checkTeamInvite(token, inviteId));
  }

  async acceptTeamInvite(inviteId: string, data: AcceptInviteRequest): Promise<TeamStatus> {
    return this.executeWithToken((token) => acceptTeamInvite(token, inviteId, data));
  }

  async removeTeamMember(userId: string): Promise<void> {
    return this.executeWithToken((token) => removeTeamMember(token, userId));
  }

  async leaveTeam(): Promise<void> {
    return this.executeWithToken((token) => leaveTeam(token));
  }

  async revokeTeamInvite(inviteId: string): Promise<void> {
    return this.executeWithToken((token) => revokeTeamInvite(token, inviteId));
  }

  async updateTeamName(name: string): Promise<UpdateTeamNameResponse> {
    return this.executeWithToken((token) => updateTeamName(token, name));
  }

  // API Credits methods
  async getApiCreditBalance(): Promise<ApiCreditBalance> {
    return this.executeWithToken((token) => fetchApiCreditBalance(token));
  }

  async getApiCreditSettings(): Promise<ApiCreditSettings> {
    return this.executeWithToken((token) => fetchApiCreditSettings(token));
  }

  async purchaseApiCredits(data: PurchaseCreditsRequest): Promise<CheckoutResponse> {
    return this.executeWithToken((token) => purchaseApiCredits(token, data));
  }

  async purchaseApiCreditsZaprite(data: PurchaseCreditsZapriteRequest): Promise<CheckoutResponse> {
    return this.executeWithToken((token) => purchaseApiCreditsZaprite(token, data));
  }

  async updateApiCreditSettings(data: UpdateCreditSettingsRequest): Promise<ApiCreditSettings> {
    return this.executeWithToken((token) => updateApiCreditSettings(token, data));
  }

  // Subscription Pass methods
  async checkPassCode(passCode: string): Promise<PassCheckResponse> {
    return checkPassCode(passCode);
  }

  async redeemPassCode(data: PassRedeemRequest): Promise<PassRedeemResponse> {
    return this.executeWithToken((token) => redeemPassCode(token, data));
  }

  async createZapriteUpgradeQuote(targetProductId: string): Promise<ZapriteUpgradeQuote> {
    return this.executeWithToken((token) => createZapriteUpgradeQuote(token, targetProductId));
  }

  async createZapriteUpgrade(
    quoteId: string,
    idempotencyKey: string,
    successUrl?: string
  ): Promise<ZapriteUpgradeCreateResponse> {
    return this.executeWithToken((token) =>
      createZapriteUpgrade(token, quoteId, idempotencyKey, successUrl)
    );
  }

  async getZapriteUpgradeStatus(upgradeId: string): Promise<ZapriteUpgradeStatusResponse> {
    return this.executeWithToken((token) => fetchZapriteUpgradeStatus(token, upgradeId));
  }
}

// Singleton instance
let billingServiceInstance: BillingService | null = null;

export function initBillingService(os: OpenSecretContextType): BillingService {
  if (!billingServiceInstance) {
    billingServiceInstance = new BillingService(os);
  } else {
    billingServiceInstance.updateOpenSecret(os);
  }
  return billingServiceInstance;
}

export function getBillingService(): BillingService {
  if (!billingServiceInstance) {
    throw new Error("Billing service not initialized. Call initBillingService first.");
  }
  return billingServiceInstance;
}
