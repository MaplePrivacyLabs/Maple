import type { ChatRuntimeKey } from "./chatRuntimeStore";

type ChatRuntimeDeletionLookup = object & {
  resolveKey: (key: ChatRuntimeKey) => ChatRuntimeKey;
  getActivityGroupId: (key: ChatRuntimeKey) => string | null | undefined;
};

type ChatRuntimeDeletionFences = {
  keys: Map<ChatRuntimeKey, number>;
  activityGroups: Map<string, number>;
  all: number;
};

const deletionFencesByStore = new WeakMap<object, ChatRuntimeDeletionFences>();

function fencesFor(store: object): ChatRuntimeDeletionFences {
  const existing = deletionFencesByStore.get(store);
  if (existing) return existing;
  const created = {
    keys: new Map<ChatRuntimeKey, number>(),
    activityGroups: new Map<string, number>(),
    all: 0
  };
  deletionFencesByStore.set(store, created);
  return created;
}

export function beginAllChatRuntimeDeletionFence(store: object): () => void {
  const fences = fencesFor(store);
  fences.all += 1;
  let released = false;
  return () => {
    if (released) return;
    released = true;
    fences.all = Math.max(0, fences.all - 1);
  };
}

export function beginChatRuntimeDeletionFence(
  store: ChatRuntimeDeletionLookup,
  key: ChatRuntimeKey
): () => void {
  const fences = fencesFor(store);
  const fencedKey = store.resolveKey(key);
  fences.keys.set(fencedKey, (fences.keys.get(fencedKey) ?? 0) + 1);
  let released = false;
  return () => {
    if (released) return;
    released = true;
    const remaining = (fences.keys.get(fencedKey) ?? 0) - 1;
    if (remaining > 0) fences.keys.set(fencedKey, remaining);
    else fences.keys.delete(fencedKey);
  };
}

export function beginChatActivityGroupDeletionFence(
  store: ChatRuntimeDeletionLookup,
  activityGroupId: string
): () => void {
  const fences = fencesFor(store);
  fences.activityGroups.set(activityGroupId, (fences.activityGroups.get(activityGroupId) ?? 0) + 1);
  let released = false;
  return () => {
    if (released) return;
    released = true;
    const remaining = (fences.activityGroups.get(activityGroupId) ?? 0) - 1;
    if (remaining > 0) fences.activityGroups.set(activityGroupId, remaining);
    else fences.activityGroups.delete(activityGroupId);
  };
}

/**
 * Fences both the runtime's cached activity group and exact conversation keys
 * discovered from the server. Exact keys cover runtimes whose metadata could
 * not be loaded, and therefore do not yet expose their project group locally.
 */
export function beginChatProjectRuntimeDeletionFence(
  store: ChatRuntimeDeletionLookup,
  activityGroupId: string,
  conversationKeys: readonly ChatRuntimeKey[]
): () => void {
  const releases = [
    beginChatActivityGroupDeletionFence(store, activityGroupId),
    ...conversationKeys.map((key) => beginChatRuntimeDeletionFence(store, key))
  ];
  let released = false;
  return () => {
    if (released) return;
    released = true;
    for (const release of releases.reverse()) release();
  };
}

export function isChatRuntimeDeletionPending(
  store: ChatRuntimeDeletionLookup,
  key: ChatRuntimeKey
): boolean {
  const fences = deletionFencesByStore.get(store);
  if (!fences) return false;
  if (fences.all > 0) return true;

  const canonicalKey = store.resolveKey(key);
  for (const [fencedKey, leaseCount] of fences.keys) {
    if (leaseCount > 0 && store.resolveKey(fencedKey) === canonicalKey) return true;
  }
  const activityGroupId = store.getActivityGroupId(canonicalKey);
  return Boolean(activityGroupId && (fences.activityGroups.get(activityGroupId) ?? 0) > 0);
}
