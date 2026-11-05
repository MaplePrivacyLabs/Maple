import { keepPreviousData, useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { getBillingService } from "./billingService";
import type { UsageQuery } from "./usageApi";

// Usage history is account-scoped, so every key carries the account id. These
// keys are deliberately not part of the per-chat-turn `billingStatus` refresh:
// history may lag a few seconds and the page refetches when it is opened.

const USAGE_STALE_MS = 60_000;
const REQUESTS_STALE_MS = 30_000;
const PRICES_STALE_MS = 60 * 60_000;
// Recent requests open short; each "Load more" fetches a larger page.
const REQUESTS_FIRST_PAGE_SIZE = 10;
const REQUESTS_MORE_PAGE_SIZE = 50;

export const usageKeys = {
  all: (accountId: string) => ["usage", accountId] as const,
  summary: (accountId: string, query: UsageQuery) =>
    [
      "usage",
      accountId,
      "summary",
      query.range,
      query.tz,
      query.periodStart ?? null,
      query.periodEnd ?? null
    ] as const,
  requests: (accountId: string) => ["usage", accountId, "requests"] as const,
  prices: ["usagePrices"] as const
};

export function useUsageSummary(
  accountId: string | undefined,
  query: UsageQuery,
  { enabled = true, keepPrevious = true }: { enabled?: boolean; keepPrevious?: boolean } = {}
) {
  return useQuery({
    queryKey: usageKeys.summary(accountId ?? "signed-out", query),
    queryFn: () => getBillingService().getUsage(query),
    enabled: !!accountId && enabled,
    staleTime: USAGE_STALE_MS,
    placeholderData: keepPrevious ? keepPreviousData : undefined
  });
}

export function useUsageRequests(accountId: string | undefined) {
  return useInfiniteQuery({
    queryKey: usageKeys.requests(accountId ?? "signed-out"),
    queryFn: ({ pageParam }) =>
      getBillingService().getUsageRequests({
        cursor: pageParam,
        limit: pageParam ? REQUESTS_MORE_PAGE_SIZE : REQUESTS_FIRST_PAGE_SIZE
      }),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (page) => page.next_cursor ?? undefined,
    enabled: !!accountId,
    staleTime: REQUESTS_STALE_MS
  });
}

export function useUsagePrices() {
  return useQuery({
    queryKey: usageKeys.prices,
    queryFn: () => getBillingService().getUsagePrices(),
    staleTime: PRICES_STALE_MS
  });
}
