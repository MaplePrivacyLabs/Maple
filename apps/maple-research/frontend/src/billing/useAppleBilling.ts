import { createContext, useContext } from "react";
import type { AppleBillingPurchaseResult } from "./appleBillingSession";
import type { AppleTransactionResponse } from "./appleBillingApi";
import type { AppleBillingLifecycleState } from "./appleBillingLifecycle";

export interface AppleBillingContextValue extends AppleBillingLifecycleState {
  available: boolean;
  purchase(productId: string): Promise<AppleBillingPurchaseResult>;
  restore(): Promise<AppleTransactionResponse[]>;
  retry(): Promise<void>;
}

async function unavailable(): Promise<never> {
  throw new Error("apple_billing_unavailable");
}

export const AppleBillingContext = createContext<AppleBillingContextValue>({
  ownerKey: null,
  available: false,
  ready: false,
  busy: false,
  recoveryError: null,
  purchase: unavailable,
  restore: unavailable,
  retry: unavailable
});

export const useAppleBilling = (): AppleBillingContextValue => useContext(AppleBillingContext);
