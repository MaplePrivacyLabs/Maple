import { useQuery } from "@tanstack/react-query";
import { useOpenSecret } from "@mapleai/sdk";

export type ApiKeySummary = {
  name: string;
  created_at: string;
  last_used_at: string | null;
};

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
