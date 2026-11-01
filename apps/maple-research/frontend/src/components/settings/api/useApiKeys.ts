import { useQuery } from "@tanstack/react-query";
import { useOpenSecret } from "@mapleai/sdk";
import { getBillingService } from "@/billing/billingService";

export type ApiKeySummary = {
  name: string;
  created_at: string;
  /** Non-secret key ID that billing reports usage by. Absent from older servers. */
  public_id?: string;
};

/** Each key's latest request that used credits, by `public_id`. */
export type ApiKeyLastUsed = Map<string, string | null>;

export function useApiKeys() {
  const { auth, listApiKeys } = useOpenSecret();

  return useQuery<ApiKeySummary[]>({
    queryKey: ["apiKeys"],
    queryFn: async () => {
      const response = await listApiKeys();
      return response.keys.sort(
        (a, b) => new Date(b.created_at).getTime() - new Date(a.created_at).getTime()
      );
    },
    enabled: !!auth.user && !auth.loading
  });
}

export function useApiKeyLastUsed() {
  const { auth } = useOpenSecret();

  return useQuery<ApiKeyLastUsed>({
    queryKey: ["apiKeyLastUsed"],
    queryFn: async () => {
      const { api_keys } = await getBillingService().getApiKeys();
      return new Map(api_keys.map((key) => [key.api_key_id, key.last_used_at]));
    },
    enabled: !!auth.user && !auth.loading
  });
}
