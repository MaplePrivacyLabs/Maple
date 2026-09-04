import { readNativeUserAuth, type NativeUserAuthState } from "@mapleai/sdk";
import { ensureNativeTransportRoot } from "@/services/nativeTransportRoot";
import { isTauriDesktop } from "@/utils/platform";

export interface MapleApiAuthSnapshot {
  userId: string;
  nativeInstanceId: string;
  revision: number;
}

function normalizeUserId(userId: string): string {
  const normalized = userId.trim().toLowerCase();
  if (!normalized) throw new Error("Maple API access requires a signed-in account");
  return normalized;
}

function readBrowserAuth(apiUrl: string): NativeUserAuthState {
  return readNativeUserAuth(apiUrl);
}

async function invokeNative<T>(command: string, args: Record<string, unknown>): Promise<T> {
  const { invoke } = await import("@tauri-apps/api/core");
  return await invoke<T>(command, args);
}

export interface MapleApiAuthBridge {
  isDesktop(): boolean;
  apiUrl(): string;
  readAuth(apiUrl: string): NativeUserAuthState;
  installRoot(apiUrl: string): Promise<void>;
  invoke<T>(command: string, args: Record<string, unknown>): Promise<T>;
}

const defaultBridge: MapleApiAuthBridge = {
  isDesktop: isTauriDesktop,
  apiUrl: () => import.meta.env.VITE_OPEN_SECRET_API_URL,
  readAuth: readBrowserAuth,
  installRoot: ensureNativeTransportRoot,
  invoke: invokeNative
};

function assertBrowserAuthority(
  state: NativeUserAuthState,
  expectedUserId: string
): asserts state is NativeUserAuthState & {
  principalId: string;
  credentials: NonNullable<NativeUserAuthState["credentials"]>;
} {
  if (
    !state.principalId ||
    normalizeUserId(state.principalId) !== expectedUserId ||
    !state.credentials ||
    !state.credentials.accessToken ||
    !state.credentials.refreshToken
  ) {
    throw new Error("Maple API access requires the current signed-in account");
  }
}

function sameBrowserAuthority(left: NativeUserAuthState, right: NativeUserAuthState): boolean {
  return (
    left.apiOrigin === right.apiOrigin &&
    left.revision === right.revision &&
    left.principalId === right.principalId &&
    left.cacheNamespaceRootBase64 === right.cacheNamespaceRootBase64
  );
}

/**
 * Installs one browser credential snapshot into Maple's native V2 client.
 *
 * Browser and native clients refresh independently after this boundary. Agent
 * calls therefore never shuttle refreshed credentials, cache roots, or auth
 * snapshots back through the WebView.
 */
export class MapleApiAuthService {
  private activeUserId: string | null = null;
  private operationTail: Promise<void> = Promise.resolve();

  constructor(private readonly bridge: MapleApiAuthBridge = defaultBridge) {}

  async activate(userId: string): Promise<void> {
    if (!this.bridge.isDesktop()) return;
    const normalizedUserId = normalizeUserId(userId);
    await this.enqueue(async () => {
      this.activeUserId = normalizedUserId;
      let nativeInstalled = false;
      try {
        const apiUrl = this.bridge.apiUrl();
        const before = this.bridge.readAuth(apiUrl);
        assertBrowserAuthority(before, normalizedUserId);

        await this.bridge.installRoot(before.apiOrigin);
        this.assertActiveUser(normalizedUserId);

        const prepared = this.bridge.readAuth(apiUrl);
        assertBrowserAuthority(prepared, normalizedUserId);
        if (!sameBrowserAuthority(before, prepared)) {
          throw new Error("Maple API authentication changed before native installation");
        }

        const snapshot = await this.bridge.invoke<MapleApiAuthSnapshot>("maple_api_set_auth", {
          request: {
            userId: normalizedUserId,
            apiUrl: prepared.apiOrigin,
            accessToken: prepared.credentials.accessToken,
            refreshToken: prepared.credentials.refreshToken,
            cacheNamespaceRootBase64: prepared.cacheNamespaceRootBase64
          }
        });
        nativeInstalled = true;
        this.assertActiveUser(normalizedUserId);
        this.assertNativeReceipt(normalizedUserId, snapshot);

        const current = this.bridge.readAuth(apiUrl);
        assertBrowserAuthority(current, normalizedUserId);
        if (!sameBrowserAuthority(prepared, current)) {
          throw new Error("Maple API authentication changed during native installation");
        }
      } catch (error) {
        if (nativeInstalled) {
          await this.bridge
            .invoke<void>("maple_api_clear_auth", { userId: normalizedUserId })
            .catch(() => undefined);
        }
        if (this.activeUserId === normalizedUserId) this.activeUserId = null;
        throw error;
      }
    });
  }

  async sync(userId: string): Promise<void> {
    if (!this.bridge.isDesktop()) return;
    const normalizedUserId = normalizeUserId(userId);
    await this.enqueue(async () => this.assertActiveUser(normalizedUserId));
  }

  async clear(userId: string): Promise<void> {
    if (!this.bridge.isDesktop()) return;
    const normalizedUserId = normalizeUserId(userId);
    await this.enqueue(async () => {
      await this.bridge.invoke<void>("maple_api_clear_auth", { userId: normalizedUserId });
      if (this.activeUserId === normalizedUserId) this.activeUserId = null;
    });
  }

  private enqueue<T>(operation: () => Promise<T>): Promise<T> {
    const result = this.operationTail.then(operation, operation);
    this.operationTail = result.then(
      () => undefined,
      () => undefined
    );
    return result;
  }

  private assertActiveUser(userId: string): void {
    if (this.activeUserId !== userId) {
      throw new Error("Maple API authentication changed before the operation started");
    }
  }

  private assertNativeReceipt(userId: string, snapshot: MapleApiAuthSnapshot): void {
    if (
      normalizeUserId(snapshot.userId) !== userId ||
      typeof snapshot.nativeInstanceId !== "string" ||
      !snapshot.nativeInstanceId ||
      !Number.isSafeInteger(snapshot.revision) ||
      snapshot.revision < 1
    ) {
      throw new Error("Maple API authentication changed while credentials were being installed");
    }
  }
}

export const mapleApiAuthService = new MapleApiAuthService();
