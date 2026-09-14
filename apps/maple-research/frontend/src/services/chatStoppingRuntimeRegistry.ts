import type { ChatRuntimeKey } from "./chatRuntimeStore";

export type ChatStoppingRuntimeRegistry = Readonly<{
  add: (key: ChatRuntimeKey, runToken: number) => void;
  delete: (key: ChatRuntimeKey, runToken: number) => void;
  getEntries: () => ReadonlyMap<ChatRuntimeKey, ReadonlySet<number>>;
  getSnapshot: () => number;
  subscribe: (listener: () => void) => () => void;
}>;

const registries = new WeakMap<object, ChatStoppingRuntimeRegistry>();

export function chatStoppingRuntimeRegistryFor(runtimeOwner: object): ChatStoppingRuntimeRegistry {
  const existing = registries.get(runtimeOwner);
  if (existing) return existing;

  let entries: ReadonlyMap<ChatRuntimeKey, ReadonlySet<number>> = new Map();
  let revision = 0;
  const listeners = new Set<() => void>();
  const publish = () => {
    revision += 1;
    for (const listener of listeners) listener();
  };
  const registry: ChatStoppingRuntimeRegistry = {
    add: (key, runToken) => {
      const currentTokens = entries.get(key);
      if (currentTokens?.has(runToken)) return;
      const nextEntries = new Map(entries);
      nextEntries.set(key, new Set(currentTokens).add(runToken));
      entries = nextEntries;
      publish();
    },
    delete: (_key, runToken) => {
      // A new conversation rekeys its active run from a draft key to a
      // conversation key. Run tokens are unique within a runtime store, so
      // remove the token from whichever pre-rekey key still owns it.
      let changed = false;
      const nextEntries = new Map<ChatRuntimeKey, ReadonlySet<number>>();
      for (const [entryKey, currentTokens] of entries) {
        if (!currentTokens.has(runToken)) {
          nextEntries.set(entryKey, currentTokens);
          continue;
        }
        changed = true;
        const nextTokens = new Set(currentTokens);
        nextTokens.delete(runToken);
        if (nextTokens.size > 0) nextEntries.set(entryKey, nextTokens);
      }
      if (!changed) return;
      entries = nextEntries;
      publish();
    },
    getEntries: () => entries,
    getSnapshot: () => revision,
    subscribe: (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    }
  };
  registries.set(runtimeOwner, registry);
  return registry;
}
