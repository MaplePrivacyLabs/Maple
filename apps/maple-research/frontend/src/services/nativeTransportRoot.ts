import { invoke } from "@tauri-apps/api/core";
import { readNativeUserAuth } from "@mapleai/sdk";

type NativeInvoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;

interface PendingInstallation {
  rootBase64: string;
  promise: Promise<void>;
}

const installedByOrigin = new Map<string, PendingInstallation>();

/**
 * Installs the SDK's durable per-origin Transport V2 cache root into native
 * process memory. The root is deliberately absent from native persistence,
 * proxy configuration, status, and events.
 */
export function ensureNativeTransportRoot(
  apiUrl: string,
  invokeNative: NativeInvoke = invoke
): Promise<void> {
  const auth = readNativeUserAuth(apiUrl);
  const existing = installedByOrigin.get(auth.apiOrigin);
  if (existing?.rootBase64 === auth.cacheNamespaceRootBase64) return existing.promise;

  const installation = invokeNative<void>("install_native_transport_root", {
    apiUrl: auth.apiOrigin,
    rootBase64: auth.cacheNamespaceRootBase64
  }).catch((error: unknown) => {
    if (installedByOrigin.get(auth.apiOrigin)?.promise === installation) {
      installedByOrigin.delete(auth.apiOrigin);
    }
    throw error;
  });
  installedByOrigin.set(auth.apiOrigin, {
    rootBase64: auth.cacheNamespaceRootBase64,
    promise: installation
  });
  return installation;
}
