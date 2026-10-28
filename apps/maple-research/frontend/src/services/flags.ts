const DEFAULT_CACHE_TTL_MS = 10 * 60 * 1000;
const DEFAULT_REQUEST_TIMEOUT_MS = 10 * 1000;
const DEV_FLAGS_BASE_URL = "https://flags-dev.opensecret.cloud";
const PROD_FLAGS_BASE_URL = "https://flags.opensecret.cloud";

export const FEATURE_FLAGS = {
  AGENT_MODE: "agent_mode"
} as const;

export type FlagValues = Readonly<Record<string, boolean>>;

export type FlagsFetch = (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>;

export interface FlagsClientOptions {
  baseUrl?: string;
  cacheTtlMs?: number;
  requestTimeoutMs?: number;
  fetchFn?: FlagsFetch;
  now?: () => number;
}

interface CacheEntry {
  expiresAt: number;
  settled: boolean;
  promise: Promise<FlagValues>;
  token: object;
  values?: FlagValues;
}

// Local override to force specific feature flags on without a server-side flag.
// Off by default; set VITE_FORCE_FEATURE_FLAGS to a comma-separated list of flag
// keys (e.g. "agent_mode") to force them enabled. Intended for local/dev builds
// only — real builds leave this unset so server flags remain authoritative.
function isForcedOn(key: string): boolean {
  const raw = import.meta.env.VITE_FORCE_FEATURE_FLAGS;
  if (typeof raw !== "string" || raw.trim() === "") return false;
  const forced = new Set(
    raw
      .split(",")
      .map((k) => k.trim())
      .filter(Boolean)
  );
  return forced.has(key.trim());
}

function defaultBaseUrl(): string {
  const configured = import.meta.env.VITE_OS_FLAGS_BASE_URL?.trim();
  if (configured) return configured;
  return import.meta.env.PROD ? PROD_FLAGS_BASE_URL : DEV_FLAGS_BASE_URL;
}

function normalizeBaseUrl(value: string): URL {
  const url = new URL(value);
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error("Feature flag base URL must use HTTP or HTTPS");
  }
  url.search = "";
  url.hash = "";
  if (!url.pathname.endsWith("/")) url.pathname += "/";
  return url;
}

function normalizeKeys(keys: readonly string[]): string[] {
  return [...new Set(keys.map((key) => key.trim()).filter(Boolean))].sort();
}

function cacheKey(userId: string, keys: readonly string[]): string {
  return JSON.stringify([userId, keys]);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parseFlagValues(value: unknown, expectedUserId: string): FlagValues {
  if (!isRecord(value) || typeof value.user_uuid !== "string" || !isRecord(value.flags)) {
    throw new Error("Feature flag service returned an invalid response");
  }
  if (value.user_uuid.toLowerCase() !== expectedUserId.toLowerCase()) {
    throw new Error("Feature flag service returned a response for another user");
  }

  const entries: Array<[string, boolean]> = [];
  for (const [key, enabled] of Object.entries(value.flags)) {
    if (typeof enabled !== "boolean") {
      throw new Error("Feature flag service returned a non-boolean flag");
    }
    entries.push([key, enabled]);
  }
  return Object.freeze(Object.fromEntries(entries));
}

export class FlagsClient {
  private readonly baseUrl: URL;
  private readonly cacheTtlMs: number;
  private readonly requestTimeoutMs: number;
  private readonly fetchFn: FlagsFetch;
  private readonly now: () => number;
  private readonly cache = new Map<string, CacheEntry>();

  constructor(options: FlagsClientOptions = {}) {
    this.baseUrl = normalizeBaseUrl(options.baseUrl ?? defaultBaseUrl());
    this.cacheTtlMs = options.cacheTtlMs ?? DEFAULT_CACHE_TTL_MS;
    this.requestTimeoutMs = options.requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS;
    this.fetchFn = options.fetchFn ?? ((input, init) => globalThis.fetch(input, init));
    this.now = options.now ?? Date.now;

    if (this.cacheTtlMs <= 0 || this.requestTimeoutMs <= 0) {
      throw new Error("Feature flag cache TTL and request timeout must be positive");
    }
  }

  getFlags(userId: string, keys: readonly string[]): Promise<FlagValues> {
    const normalizedUserId = userId.trim();
    if (!normalizedUserId) {
      return Promise.reject(new Error("Feature flag lookup requires a user ID"));
    }

    const normalizedKeys = normalizeKeys(keys);
    if (normalizedKeys.length === 0) return Promise.resolve(Object.freeze({}));

    const now = this.now();
    this.evictExpired(now);
    const key = cacheKey(normalizedUserId, normalizedKeys);
    const existing = this.cache.get(key);
    if (existing && (!existing.settled || existing.expiresAt > now)) {
      return existing.promise;
    }

    const token = {};
    const promise = this.requestFlags(normalizedUserId, normalizedKeys).then(
      (flags) => {
        const current = this.cache.get(key);
        if (current?.token === token) {
          current.settled = true;
          current.expiresAt = this.now() + this.cacheTtlMs;
          current.values = flags;
        }
        return flags;
      },
      (error: unknown) => {
        if (this.cache.get(key)?.token === token) this.cache.delete(key);
        throw error;
      }
    );
    const entry = { expiresAt: Number.POSITIVE_INFINITY, settled: false, promise, token };
    this.cache.set(key, entry);
    return promise;
  }

  async isEnabled(userId: string, key: string): Promise<boolean> {
    if (isForcedOn(key)) return true;
    const flags = await this.getFlags(userId, [key]);
    return flags[key.trim()] === true;
  }

  peekIsEnabled(userId: string, key: string): boolean | undefined {
    if (isForcedOn(key)) return true;
    const normalizedUserId = userId.trim();
    const [normalizedKey] = normalizeKeys([key]);
    if (!normalizedUserId || !normalizedKey) return undefined;

    this.evictExpired(this.now());
    const entry = this.cache.get(cacheKey(normalizedUserId, [normalizedKey]));
    if (!entry?.settled || !entry.values) return undefined;
    return entry.values[normalizedKey] === true;
  }

  private evictExpired(now: number): void {
    for (const [key, entry] of this.cache) {
      if (entry.settled && entry.expiresAt <= now) this.cache.delete(key);
    }
  }

  private async requestFlags(userId: string, keys: readonly string[]): Promise<FlagValues> {
    const url = new URL(`v1/users/${encodeURIComponent(userId)}/flags`, this.baseUrl);
    url.searchParams.set("keys", keys.join(","));

    const controller = new AbortController();
    const timeout = globalThis.setTimeout(() => controller.abort(), this.requestTimeoutMs);
    try {
      const response = await this.fetchFn(url, {
        method: "GET",
        headers: { Accept: "application/json" },
        credentials: "omit",
        cache: "no-store",
        signal: controller.signal
      });
      if (!response.ok) {
        throw new Error(`Feature flag request failed with status ${response.status}`);
      }
      return parseFlagValues(await response.json(), userId);
    } finally {
      globalThis.clearTimeout(timeout);
    }
  }
}

export const flagsClient = new FlagsClient();
