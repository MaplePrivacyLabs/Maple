export type ChatRuntimeKey = string;

const EMPTY_CHAT_RUNTIME_KEYS: readonly ChatRuntimeKey[] = Object.freeze([]);

export type DraftChatRuntimeKey = `draft:${string}`;
export type ConversationChatRuntimeKey = `conversation:${string}`;

let fallbackDraftKeySequence = 0;

export function createChatDraftKey(id?: string): DraftChatRuntimeKey {
  const generatedId =
    id ??
    globalThis.crypto?.randomUUID?.() ??
    `${Date.now().toString(36)}-${(++fallbackDraftKeySequence).toString(36)}`;
  if (!generatedId) throw new Error("Chat draft key ID must not be empty");
  return `draft:${generatedId}`;
}

export function createConversationChatKey(conversationId: string): ConversationChatRuntimeKey {
  if (!conversationId) throw new Error("Conversation ID must not be empty");
  return `conversation:${conversationId}`;
}

export type ChatRuntimeSnapshot<TConversation, TMessage, TComposer> = Readonly<{
  key: ChatRuntimeKey;
  revision: number;
  conversation: TConversation | null;
  messages: readonly TMessage[];
  composer: TComposer;
  isGenerating: boolean;
  currentResponseId: string | undefined;
  error: string | null;
  lastSeenItemId: string | undefined;
  assistantStreaming: boolean;
  historyLoaded: boolean;
  runToken: number | null;
}>;

export type ChatRuntimeInitialState<TConversation, TMessage, TComposer> = Partial<
  Pick<
    ChatRuntimeSnapshot<TConversation, TMessage, TComposer>,
    "conversation" | "messages" | "composer" | "error" | "lastSeenItemId" | "historyLoaded"
  >
>;

type ChatRuntimeRunOwnedField = "isGenerating" | "currentResponseId" | "assistantStreaming";

export type ChatRuntimeSafeSnapshot<TConversation, TMessage, TComposer> = Omit<
  ChatRuntimeSnapshot<TConversation, TMessage, TComposer>,
  ChatRuntimeRunOwnedField
>;

export type ChatRuntimeStateUpdate<TConversation, TMessage, TComposer> = Omit<
  ChatRuntimeSafeSnapshot<TConversation, TMessage, TComposer>,
  "key" | "revision" | "runToken"
>;

export type ChatRuntimeRunStateUpdate<TConversation, TMessage, TComposer> = Omit<
  ChatRuntimeSnapshot<TConversation, TMessage, TComposer>,
  "key" | "revision" | "runToken"
>;

export type ChatRuntimeActiveRunSnapshot<TConversation, TMessage, TComposer> = Omit<
  ChatRuntimeSnapshot<TConversation, TMessage, TComposer>,
  "isGenerating"
>;

export type ChatRuntimeActiveRunStateUpdate<TConversation, TMessage, TComposer> = Omit<
  ChatRuntimeRunStateUpdate<TConversation, TMessage, TComposer>,
  "isGenerating"
> & { readonly isGenerating?: never };

export type ChatRuntimeUpdater<TConversation, TMessage, TComposer> = (
  snapshot: ChatRuntimeSafeSnapshot<TConversation, TMessage, TComposer>
) => ChatRuntimeStateUpdate<TConversation, TMessage, TComposer>;

export type ChatRuntimeRunUpdater<TConversation, TMessage, TComposer> = (
  snapshot: ChatRuntimeActiveRunSnapshot<TConversation, TMessage, TComposer>
) => ChatRuntimeActiveRunStateUpdate<TConversation, TMessage, TComposer>;

export type ChatRuntimeIdleDestinationMerger<TConversation, TMessage, TComposer> = (
  source: ChatRuntimeActiveRunSnapshot<TConversation, TMessage, TComposer>,
  destination: ChatRuntimeSnapshot<TConversation, TMessage, TComposer>
) => ChatRuntimeActiveRunStateUpdate<TConversation, TMessage, TComposer>;

export type ChatRuntimeRunRekeyResult =
  | Readonly<{
      status: "migrated";
      key: ChatRuntimeKey;
      adoptedExistingDestination: boolean;
      destinationWasSelected: boolean;
    }>
  | Readonly<{ status: "source_stale" }>
  | Readonly<{ status: "destination_active"; key: ChatRuntimeKey }>;

export type ChatRuntimeRunHandle = Readonly<{
  key: ChatRuntimeKey;
  token: number;
  controller: AbortController;
  signal: AbortSignal;
}>;

export type ChatRuntimeRunContext = Readonly<{
  groupId?: string | null;
}>;

export type ChatRuntimeVisibleLease = Readonly<{
  owner: object;
  sequence: number;
}>;

export type CancelledChatRuntimeRun = Readonly<{
  key: ChatRuntimeKey;
  token: number;
  responseId: string | undefined;
  controller: AbortController;
}>;

export type ChatRuntimeStoreOptions<TConversation, TMessage, TComposer> = Readonly<{
  createComposer: () => TComposer;
  maxInactiveCompletedEntries?: number;
  canEvict?: (snapshot: ChatRuntimeSnapshot<TConversation, TMessage, TComposer>) => boolean;
  disposeEntry?: (
    snapshot: ChatRuntimeSnapshot<TConversation, TMessage, TComposer>,
    reason: "evicted" | "deleted" | "disposed"
  ) => void;
}>;

type ActiveRun = {
  token: number;
  controller: AbortController;
  groupId: string | null;
};

type RuntimeEntry<TConversation, TMessage, TComposer> = {
  snapshot: ChatRuntimeSnapshot<TConversation, TMessage, TComposer>;
  activeRun: ActiveRun | null;
  groupId: string | null;
  lastTouched: number;
};

/**
 * In-memory ownership boundary for Chat state that must survive navigation.
 *
 * The store deliberately has no React or OpenAI SDK dependency. A UI can read it
 * with useSyncExternalStore by subscribing to `subscribe` and using
 * `getSubscriberRevision` as the snapshot. Network loops retain the key and run
 * token returned by `beginRun`, so an offscreen chat can keep receiving events
 * without writing into the currently selected chat.
 */
export class ChatRuntimeStore<TConversation, TMessage, TComposer> {
  private readonly entries = new Map<
    ChatRuntimeKey,
    RuntimeEntry<TConversation, TMessage, TComposer>
  >();
  private readonly aliases = new Map<ChatRuntimeKey, ChatRuntimeKey>();
  private readonly listeners = new Set<() => void>();
  private readonly activeRunListeners = new Set<() => void>();
  private readonly completedUnreadListeners = new Set<() => void>();
  private readonly completedUnreadGroups = new Map<ChatRuntimeKey, string | null>();
  private readonly rememberedDraftKeys = new Map<string | null, DraftChatRuntimeKey>();
  private readonly createComposer: () => TComposer;
  private readonly maxInactiveCompletedEntries: number;
  private readonly canEvict: (
    snapshot: ChatRuntimeSnapshot<TConversation, TMessage, TComposer>
  ) => boolean;
  private readonly disposeEntry:
    | ((
        snapshot: ChatRuntimeSnapshot<TConversation, TMessage, TComposer>,
        reason: "evicted" | "deleted" | "disposed"
      ) => void)
    | undefined;
  private activeKey: ChatRuntimeKey | null = null;
  private activeRunKeysSnapshot = EMPTY_CHAT_RUNTIME_KEYS;
  private completedUnreadKeysSnapshot = EMPTY_CHAT_RUNTIME_KEYS;
  private visibleChatLease: ChatRuntimeVisibleLease | null = null;
  private visibleChatKey: ChatRuntimeKey | null = null;
  private nextVisibleChatLeaseSequence = 0;
  private subscriberRevision = 0;
  private nextRunToken = 0;
  private touchRevision = 0;
  private disposed = false;

  constructor(options: ChatRuntimeStoreOptions<TConversation, TMessage, TComposer>) {
    this.createComposer = options.createComposer;
    this.maxInactiveCompletedEntries = options.maxInactiveCompletedEntries ?? 20;
    if (
      !Number.isInteger(this.maxInactiveCompletedEntries) ||
      this.maxInactiveCompletedEntries < 0
    ) {
      throw new Error("maxInactiveCompletedEntries must be a non-negative integer");
    }
    this.canEvict =
      options.canEvict ??
      ((snapshot) =>
        !snapshot.isGenerating && !snapshot.assistantStreaming && snapshot.runToken === null);
    this.disposeEntry = options.disposeEntry;
  }

  readonly subscribe = (listener: () => void): (() => void) => {
    this.assertNotDisposed();
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  readonly getSubscriberRevision = (): number => this.subscriberRevision;

  readonly subscribeActiveRuns = (listener: () => void): (() => void) => {
    this.assertNotDisposed();
    this.activeRunListeners.add(listener);
    return () => this.activeRunListeners.delete(listener);
  };

  readonly getActiveRunKeys = (): readonly ChatRuntimeKey[] => this.activeRunKeysSnapshot;

  readonly subscribeCompletedUnread = (listener: () => void): (() => void) => {
    this.assertNotDisposed();
    this.completedUnreadListeners.add(listener);
    return () => this.completedUnreadListeners.delete(listener);
  };

  readonly getCompletedUnreadKeys = (): readonly ChatRuntimeKey[] =>
    this.completedUnreadKeysSnapshot;

  getActiveRunGroupId(key: ChatRuntimeKey): string | null | undefined {
    return this.entries.get(this.resolveKey(key))?.activeRun?.groupId;
  }

  getCompletedUnreadGroupId(key: ChatRuntimeKey): string | null | undefined {
    return this.completedUnreadGroups.get(this.resolveKey(key));
  }

  getActivityGroupId(key: ChatRuntimeKey): string | null | undefined {
    const canonicalKey = this.resolveKey(key);
    return this.entries.get(canonicalKey)?.groupId ?? this.completedUnreadGroups.get(canonicalKey);
  }

  rememberDraftKey(scopeId: string | null, key: DraftChatRuntimeKey): void {
    this.assertNotDisposed();
    this.rememberedDraftKeys.set(scopeId, key);
  }

  getRememberedDraftKey(scopeId: string | null): DraftChatRuntimeKey | null {
    this.assertNotDisposed();
    return this.rememberedDraftKeys.get(scopeId) ?? null;
  }

  forgetRememberedDraftKey(scopeId: string | null, expectedKey?: DraftChatRuntimeKey): boolean {
    this.assertNotDisposed();
    const rememberedKey = this.rememberedDraftKeys.get(scopeId);
    if (
      rememberedKey === undefined ||
      (expectedKey !== undefined && rememberedKey !== expectedKey)
    ) {
      return false;
    }
    return this.rememberedDraftKeys.delete(scopeId);
  }

  claimVisibleChat(owner: object, key: ChatRuntimeKey): ChatRuntimeVisibleLease {
    this.assertNotDisposed();
    const canonicalKey = this.resolveKey(key);
    const lease = Object.freeze({
      owner,
      sequence: ++this.nextVisibleChatLeaseSequence
    });
    this.visibleChatLease = lease;
    this.visibleChatKey = canonicalKey;
    if (this.completedUnreadGroups.delete(canonicalKey)) this.publish();
    return lease;
  }

  releaseVisibleChat(lease: ChatRuntimeVisibleLease): void {
    if (this.disposed) return;
    if (this.visibleChatLease !== lease) return;
    this.visibleChatLease = null;
    this.visibleChatKey = null;
  }

  isChatVisible(key: ChatRuntimeKey): boolean {
    return (
      this.visibleChatLease !== null &&
      this.visibleChatKey !== null &&
      this.resolveKey(this.visibleChatKey) === this.resolveKey(key)
    );
  }

  markCompletionRead(key: ChatRuntimeKey): boolean {
    this.assertNotDisposed();
    if (!this.completedUnreadGroups.delete(this.resolveKey(key))) return false;
    this.publish();
    return true;
  }

  updateActivityGroup(key: ChatRuntimeKey, groupId: string | null): boolean {
    this.assertNotDisposed();
    const canonicalKey = this.resolveKey(key);
    const entry = this.entries.get(canonicalKey);
    const activeRun = entry?.activeRun;
    let entryGroupChanged = false;
    let activeGroupChanged = false;
    let unreadGroupChanged = false;

    if (entry && entry.groupId !== groupId) {
      entry.groupId = groupId;
      entryGroupChanged = true;
    }
    if (activeRun && activeRun.groupId !== groupId) {
      activeRun.groupId = groupId;
      activeGroupChanged = true;
    }
    if (
      this.completedUnreadGroups.has(canonicalKey) &&
      this.completedUnreadGroups.get(canonicalKey) !== groupId
    ) {
      this.completedUnreadGroups.set(canonicalKey, groupId);
      unreadGroupChanged = true;
    }

    if (entryGroupChanged || activeGroupChanged || unreadGroupChanged) {
      this.publish(activeGroupChanged, unreadGroupChanged);
    }
    return entryGroupChanged || activeGroupChanged || unreadGroupChanged;
  }

  reassignActivityGroup(fromGroupId: string, toGroupId: string | null): boolean {
    this.assertNotDisposed();
    let entryGroupChanged = false;
    let activeGroupChanged = false;
    let unreadGroupChanged = false;

    for (const entry of this.entries.values()) {
      if (entry.groupId === fromGroupId) {
        entry.groupId = toGroupId;
        entryGroupChanged = true;
      }
      if (entry.activeRun?.groupId === fromGroupId) {
        entry.activeRun.groupId = toGroupId;
        activeGroupChanged = true;
      }
    }
    for (const [key, groupId] of this.completedUnreadGroups) {
      if (groupId === fromGroupId) {
        this.completedUnreadGroups.set(key, toGroupId);
        unreadGroupChanged = true;
      }
    }

    if (entryGroupChanged || activeGroupChanged || unreadGroupChanged) {
      this.publish(activeGroupChanged, unreadGroupChanged);
    }
    return entryGroupChanged || activeGroupChanged || unreadGroupChanged;
  }

  deleteActivityGroup(groupId: string): readonly ChatRuntimeKey[] {
    this.assertNotDisposed();
    const keys = new Set<ChatRuntimeKey>();

    for (const [key, entry] of this.entries) {
      if (entry.groupId === groupId || entry.activeRun?.groupId === groupId) keys.add(key);
    }
    for (const [key, completedGroupId] of this.completedUnreadGroups) {
      if (completedGroupId === groupId) keys.add(key);
    }

    this.rememberedDraftKeys.delete(groupId);
    for (const key of keys) this.delete(key);
    return Object.freeze(Array.from(keys));
  }

  /** Clears every account-scoped runtime while keeping this provider's store reusable. */
  clearAll(): readonly ChatRuntimeKey[] {
    this.assertNotDisposed();
    const currentEntries = Array.from(this.entries.entries());
    const keys = Object.freeze(currentEntries.map(([key]) => key));
    const hadState =
      currentEntries.length > 0 ||
      this.aliases.size > 0 ||
      this.completedUnreadGroups.size > 0 ||
      this.rememberedDraftKeys.size > 0 ||
      this.activeKey !== null ||
      this.visibleChatLease !== null;
    if (!hadState) return keys;

    this.entries.clear();
    this.aliases.clear();
    this.completedUnreadGroups.clear();
    this.rememberedDraftKeys.clear();
    this.activeKey = null;
    this.visibleChatLease = null;
    this.visibleChatKey = null;

    this.runAll([
      ...currentEntries.flatMap(([, entry]) =>
        entry.activeRun ? [() => entry.activeRun!.controller.abort()] : []
      ),
      ...currentEntries.map(
        ([, entry]) =>
          () =>
            this.disposeEntry?.(entry.snapshot, "deleted")
      ),
      () => this.publish(true, true)
    ]);
    return keys;
  }

  subscribeKey(key: ChatRuntimeKey, listener: () => void): () => void {
    let lastSnapshot = this.get(key);
    return this.subscribe(() => {
      const nextSnapshot = this.get(key);
      if (nextSnapshot === lastSnapshot) return;
      lastSnapshot = nextSnapshot;
      listener();
    });
  }

  getActiveKey(): ChatRuntimeKey | null {
    return this.activeKey;
  }

  getActive(): ChatRuntimeSnapshot<TConversation, TMessage, TComposer> | undefined {
    return this.activeKey === null ? undefined : this.entries.get(this.activeKey)?.snapshot;
  }

  resolveKey(key: ChatRuntimeKey): ChatRuntimeKey {
    let current = key;
    const path: ChatRuntimeKey[] = [];
    const visited = new Set<ChatRuntimeKey>();

    while (this.aliases.has(current)) {
      if (visited.has(current)) {
        throw new Error(`Chat runtime alias cycle detected at ${current}`);
      }
      visited.add(current);
      path.push(current);
      current = this.aliases.get(current)!;
    }

    for (const alias of path) {
      this.aliases.set(alias, current);
    }
    return current;
  }

  get(key: ChatRuntimeKey): ChatRuntimeSnapshot<TConversation, TMessage, TComposer> | undefined {
    return this.entries.get(this.resolveKey(key))?.snapshot;
  }

  /** Read-only account snapshot used for aggregate resource admission checks. */
  getSnapshots(): readonly ChatRuntimeSnapshot<TConversation, TMessage, TComposer>[] {
    return Object.freeze(Array.from(this.entries.values(), (entry) => entry.snapshot));
  }

  ensure(
    key: ChatRuntimeKey,
    initial: ChatRuntimeInitialState<TConversation, TMessage, TComposer> = {}
  ): ChatRuntimeSnapshot<TConversation, TMessage, TComposer> {
    this.assertNotDisposed();
    const canonicalKey = this.resolveKey(key);
    const existing = this.entries.get(canonicalKey);
    if (existing) return existing.snapshot;

    const entry = this.createEntry(canonicalKey, initial);
    this.entries.set(canonicalKey, entry);
    this.evictInactiveCompletedEntries(canonicalKey);
    this.publish();
    return entry.snapshot;
  }

  select(
    key: ChatRuntimeKey,
    initial: ChatRuntimeInitialState<TConversation, TMessage, TComposer> = {}
  ): ChatRuntimeSnapshot<TConversation, TMessage, TComposer> {
    this.assertNotDisposed();
    const canonicalKey = this.resolveKey(key);
    let entry = this.entries.get(canonicalKey);
    let changed = false;
    if (!entry) {
      entry = this.createEntry(canonicalKey, initial);
      this.entries.set(canonicalKey, entry);
      changed = true;
    }

    this.touch(entry);
    if (this.activeKey !== canonicalKey) {
      this.activeKey = canonicalKey;
      changed = true;
    }
    changed = this.evictInactiveCompletedEntries() || changed;
    if (changed) this.publish();
    return entry.snapshot;
  }

  clearSelection(): void {
    this.assertNotDisposed();
    if (this.activeKey === null) return;
    this.activeKey = null;
    this.evictInactiveCompletedEntries();
    this.publish();
  }

  update(
    key: ChatRuntimeKey,
    updater: ChatRuntimeUpdater<TConversation, TMessage, TComposer>
  ): ChatRuntimeSnapshot<TConversation, TMessage, TComposer> {
    this.assertNotDisposed();
    const canonicalKey = this.resolveKey(key);
    const entry = this.requireEntry(canonicalKey);
    const previous = entry.snapshot;
    const updated = updater(previous);
    entry.snapshot = this.updatedSnapshot(
      previous,
      { ...previous, ...updated },
      {
        isGenerating: previous.isGenerating,
        currentResponseId: previous.currentResponseId,
        assistantStreaming: previous.assistantStreaming,
        runToken: previous.runToken
      }
    );
    this.touch(entry);
    this.evictInactiveCompletedEntries();
    this.publish();
    return entry.snapshot;
  }

  updateForRun(
    key: ChatRuntimeKey,
    token: number,
    updater: ChatRuntimeRunUpdater<TConversation, TMessage, TComposer>
  ): boolean {
    if (this.disposed) return false;
    const canonicalKey = this.resolveKey(key);
    const entry = this.entries.get(canonicalKey);
    if (!entry || entry.activeRun?.token !== token) return false;

    const updated = updater(entry.snapshot);
    entry.snapshot = this.updatedSnapshot(
      entry.snapshot,
      { ...entry.snapshot, ...updated },
      {
        isGenerating: true,
        currentResponseId: updated.currentResponseId,
        assistantStreaming: updated.assistantStreaming,
        runToken: token
      }
    );
    this.touch(entry);
    this.evictInactiveCompletedEntries();
    this.publish();
    return true;
  }

  beginRun(key: ChatRuntimeKey, context: ChatRuntimeRunContext = {}): ChatRuntimeRunHandle {
    this.assertNotDisposed();
    const canonicalKey = this.resolveKey(key);
    const entry = this.entries.get(canonicalKey) ?? this.createAndStoreEntry(canonicalKey);
    if (entry.activeRun) {
      throw new Error(`Chat runtime already has an active run: ${canonicalKey}`);
    }
    const controller = new AbortController();
    const token = ++this.nextRunToken;

    this.completedUnreadGroups.delete(canonicalKey);
    const groupId = context.groupId === undefined ? entry.groupId : context.groupId;
    entry.groupId = groupId;
    entry.activeRun = { token, controller, groupId };
    entry.snapshot = this.updatedSnapshot(
      entry.snapshot,
      {
        ...entry.snapshot,
        isGenerating: true,
        currentResponseId: undefined,
        error: null,
        assistantStreaming: false
      },
      {
        isGenerating: true,
        currentResponseId: undefined,
        assistantStreaming: false,
        runToken: token
      }
    );
    this.touch(entry);
    this.publish();

    return { key: canonicalKey, token, controller, signal: controller.signal };
  }

  isRunCurrent(key: ChatRuntimeKey, token: number): boolean {
    return this.entries.get(this.resolveKey(key))?.activeRun?.token === token;
  }

  setCurrentResponseId(key: ChatRuntimeKey, token: number, responseId: string): boolean {
    return this.updateForRun(key, token, (snapshot) => ({
      ...snapshot,
      currentResponseId: responseId
    }));
  }

  setAssistantStreaming(key: ChatRuntimeKey, token: number, streaming: boolean): boolean {
    return this.updateForRun(key, token, (snapshot) => ({
      ...snapshot,
      assistantStreaming: streaming
    }));
  }

  finishRun(
    key: ChatRuntimeKey,
    token: number,
    updater?: ChatRuntimeRunUpdater<TConversation, TMessage, TComposer>
  ): boolean {
    return this.settleRun(key, token, false, updater);
  }

  completeRun(
    key: ChatRuntimeKey,
    token: number,
    updater?: ChatRuntimeRunUpdater<TConversation, TMessage, TComposer>
  ): boolean {
    return this.settleRun(key, token, true, updater);
  }

  /**
   * Reconciles a durably completed response while terminating a stream that
   * can no longer provide useful ownership updates. Ownership is cleared before
   * abort listeners run, preserving the same stale-token guarantee as cancel.
   */
  completeRunAndAbort(
    key: ChatRuntimeKey,
    token: number,
    updater?: ChatRuntimeRunUpdater<TConversation, TMessage, TComposer>
  ): boolean {
    return this.settleRun(key, token, true, updater, true);
  }

  private settleRun(
    key: ChatRuntimeKey,
    token: number,
    completedSuccessfully: boolean,
    updater?: ChatRuntimeRunUpdater<TConversation, TMessage, TComposer>,
    abortController = false
  ): boolean {
    if (this.disposed) return false;
    const canonicalKey = this.resolveKey(key);
    const entry = this.entries.get(canonicalKey);
    if (!entry || entry.activeRun?.token !== token) return false;

    const completed = updater ? { ...entry.snapshot, ...updater(entry.snapshot) } : entry.snapshot;
    const completedGroupId = entry.activeRun.groupId;
    const run = entry.activeRun;
    entry.activeRun = null;
    entry.snapshot = this.updatedSnapshot(
      entry.snapshot,
      {
        ...completed,
        isGenerating: false,
        currentResponseId: undefined,
        assistantStreaming: false
      },
      {
        isGenerating: false,
        currentResponseId: undefined,
        assistantStreaming: false,
        runToken: null
      }
    );
    const visibleChatKey =
      this.visibleChatLease && this.visibleChatKey ? this.resolveKey(this.visibleChatKey) : null;
    if (completedSuccessfully && visibleChatKey !== canonicalKey) {
      this.completedUnreadGroups.set(canonicalKey, completedGroupId);
    } else {
      this.completedUnreadGroups.delete(canonicalKey);
    }
    this.touch(entry);
    this.evictInactiveCompletedEntries();
    if (abortController) run.controller.abort();
    this.publish();
    return true;
  }

  cancelRun(key: ChatRuntimeKey, token: number): CancelledChatRuntimeRun | null {
    if (this.disposed) return null;
    const canonicalKey = this.resolveKey(key);
    const entry = this.entries.get(canonicalKey);
    const run = entry?.activeRun;
    if (!entry || !run || run.token !== token) return null;

    const responseId = entry.snapshot.currentResponseId;
    this.completedUnreadGroups.delete(canonicalKey);
    entry.activeRun = null;
    entry.snapshot = this.updatedSnapshot(
      entry.snapshot,
      {
        ...entry.snapshot,
        isGenerating: false,
        currentResponseId: undefined,
        assistantStreaming: false
      },
      {
        isGenerating: false,
        currentResponseId: undefined,
        assistantStreaming: false,
        runToken: null
      }
    );
    this.touch(entry);
    // Clear ownership first so a synchronous abort listener is stale-fenced.
    this.runAll([
      () => run.controller.abort(),
      () => {
        this.evictInactiveCompletedEntries();
      },
      () => this.publish()
    ]);
    return { key: canonicalKey, token: run.token, responseId, controller: run.controller };
  }

  rekey(
    fromKey: ChatRuntimeKey,
    toKey: ChatRuntimeKey,
    expectedRunToken: number
  ): ChatRuntimeKey | null {
    if (this.disposed) return null;
    const canonicalFrom = this.resolveKey(fromKey);
    const entry = this.entries.get(canonicalFrom);
    if (!entry || entry.activeRun?.token !== expectedRunToken) return null;

    const canonicalTo = this.resolveKey(toKey);
    if (canonicalFrom === canonicalTo) return canonicalTo;
    if (canonicalTo !== toKey) {
      throw new Error(`Cannot rekey chat runtime to aliased key ${toKey}`);
    }

    if (this.entries.has(canonicalTo)) {
      throw new Error(`Cannot rekey chat runtime: ${canonicalTo} already exists`);
    }

    return this.moveRunEntry(fromKey, canonicalFrom, canonicalTo, entry);
  }

  /**
   * Moves an owned run to its server conversation key without exposing a
   * delete/recreate gap when navigation discovered that conversation first.
   * An idle destination is folded into the source by the caller-provided
   * merger; its resources become part of the surviving entry and are not
   * disposed. A destination with its own run is never replaced.
   */
  rekeyRunAdoptingIdleDestination(
    fromKey: ChatRuntimeKey,
    toKey: ChatRuntimeKey,
    expectedRunToken: number,
    mergeIdleDestination: ChatRuntimeIdleDestinationMerger<TConversation, TMessage, TComposer>
  ): ChatRuntimeRunRekeyResult {
    if (this.disposed) return { status: "source_stale" };
    const canonicalFrom = this.resolveKey(fromKey);
    const sourceEntry = this.entries.get(canonicalFrom);
    if (!sourceEntry || sourceEntry.activeRun?.token !== expectedRunToken) {
      return { status: "source_stale" };
    }

    const canonicalTo = this.resolveKey(toKey);
    if (canonicalFrom === canonicalTo) {
      return {
        status: "migrated",
        key: canonicalTo,
        adoptedExistingDestination: false,
        destinationWasSelected: this.activeKey === canonicalTo
      };
    }
    if (canonicalTo !== toKey) {
      throw new Error(`Cannot rekey chat runtime to aliased key ${toKey}`);
    }

    const destinationEntry = this.entries.get(canonicalTo);
    if (destinationEntry?.activeRun) {
      return { status: "destination_active", key: canonicalTo };
    }

    const destinationWasSelected = this.activeKey === canonicalTo;
    const mergedUpdate = destinationEntry
      ? mergeIdleDestination(sourceEntry.snapshot, destinationEntry.snapshot)
      : undefined;
    const key = this.moveRunEntry(fromKey, canonicalFrom, canonicalTo, sourceEntry, mergedUpdate);

    return {
      status: "migrated",
      key,
      adoptedExistingDestination: destinationEntry !== undefined,
      destinationWasSelected
    };
  }

  private moveRunEntry(
    fromKey: ChatRuntimeKey,
    canonicalFrom: ChatRuntimeKey,
    canonicalTo: ChatRuntimeKey,
    entry: RuntimeEntry<TConversation, TMessage, TComposer>,
    mergedUpdate?: ChatRuntimeActiveRunStateUpdate<TConversation, TMessage, TComposer>
  ): ChatRuntimeKey {
    const aliasesForEntry = Array.from(this.aliases.keys()).filter(
      (alias) => this.resolveKey(alias) === canonicalFrom
    );
    const sourceWasSelected = this.activeKey === canonicalFrom;
    const destinationWasSelected = this.activeKey === canonicalTo;

    this.forgetDraftKeysResolvingTo(canonicalFrom);

    if (mergedUpdate) {
      const previous = entry.snapshot;
      entry.snapshot = this.updatedSnapshot(
        previous,
        { ...previous, ...mergedUpdate },
        {
          isGenerating: true,
          currentResponseId: previous.currentResponseId,
          assistantStreaming: previous.assistantStreaming,
          runToken: entry.activeRun!.token
        }
      );
    }

    this.completedUnreadGroups.delete(canonicalFrom);
    this.completedUnreadGroups.delete(canonicalTo);

    this.entries.delete(canonicalFrom);
    // An adopted idle destination is replaced atomically. Its resource-bearing
    // state must have been transferred by mergedUpdate, so no disposal callback
    // runs and subscribers never observe a missing destination.
    this.entries.delete(canonicalTo);
    entry.snapshot = Object.freeze({
      ...entry.snapshot,
      key: canonicalTo,
      revision: entry.snapshot.revision + 1
    });
    this.touch(entry);
    this.entries.set(canonicalTo, entry);

    this.aliases.set(canonicalFrom, canonicalTo);
    this.aliases.set(fromKey, canonicalTo);
    for (const alias of aliasesForEntry) {
      this.aliases.set(alias, canonicalTo);
    }
    if (sourceWasSelected || destinationWasSelected) {
      this.activeKey = canonicalTo;
    }
    if (
      this.visibleChatKey !== null &&
      (this.resolveKey(this.visibleChatKey) === canonicalFrom ||
        this.resolveKey(this.visibleChatKey) === canonicalTo)
    ) {
      this.visibleChatKey = canonicalTo;
    }

    this.publish();
    return canonicalTo;
  }

  delete(key: ChatRuntimeKey): boolean {
    this.assertNotDisposed();
    const canonicalKey = this.resolveKey(key);
    const entry = this.entries.get(canonicalKey);
    if (!entry) {
      const removedRememberedDraft = this.forgetDraftKeysResolvingTo(canonicalKey);
      const removedAlias = this.detachAliases(canonicalKey);
      const removedUnread = this.completedUnreadGroups.delete(canonicalKey);
      if (removedRememberedDraft || removedAlias || removedUnread) this.publish();
      return removedRememberedDraft || removedAlias || removedUnread;
    }

    this.completedUnreadGroups.delete(canonicalKey);
    this.forgetDraftKeysResolvingTo(canonicalKey);
    this.detachEntry(canonicalKey);
    if (this.activeKey === canonicalKey) this.activeKey = null;
    if (this.visibleChatKey && this.resolveKey(this.visibleChatKey) === canonicalKey) {
      this.visibleChatLease = null;
      this.visibleChatKey = null;
    }
    this.runAll([
      () => entry.activeRun?.controller.abort(),
      () => this.disposeEntry?.(entry.snapshot, "deleted"),
      () => this.publish()
    ]);
    return true;
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    const currentEntries = Array.from(this.entries.values());
    const currentListeners = Array.from(this.listeners);
    const currentActiveRunListeners = Array.from(this.activeRunListeners);
    const currentCompletedUnreadListeners = Array.from(this.completedUnreadListeners);
    this.entries.clear();
    this.aliases.clear();
    this.completedUnreadGroups.clear();
    this.rememberedDraftKeys.clear();
    this.activeKey = null;
    this.visibleChatLease = null;
    this.visibleChatKey = null;
    this.activeRunKeysSnapshot = EMPTY_CHAT_RUNTIME_KEYS;
    this.completedUnreadKeysSnapshot = EMPTY_CHAT_RUNTIME_KEYS;
    this.listeners.clear();
    this.activeRunListeners.clear();
    this.completedUnreadListeners.clear();
    this.subscriberRevision += 1;

    this.runAll([
      ...currentEntries.flatMap((entry) =>
        entry.activeRun ? [() => entry.activeRun!.controller.abort()] : []
      ),
      ...currentEntries.map((entry) => () => this.disposeEntry?.(entry.snapshot, "disposed")),
      () => this.notifyListeners(currentListeners),
      () => this.notifyListeners(currentActiveRunListeners),
      () => this.notifyListeners(currentCompletedUnreadListeners)
    ]);
  }

  private createEntry(
    key: ChatRuntimeKey,
    initial: ChatRuntimeInitialState<TConversation, TMessage, TComposer>
  ): RuntimeEntry<TConversation, TMessage, TComposer> {
    return {
      snapshot: Object.freeze({
        key,
        revision: 0,
        conversation: initial.conversation ?? null,
        messages: [...(initial.messages ?? [])],
        composer: initial.composer ?? this.createComposer(),
        isGenerating: false,
        currentResponseId: undefined,
        error: initial.error ?? null,
        lastSeenItemId: initial.lastSeenItemId,
        assistantStreaming: false,
        historyLoaded: initial.historyLoaded ?? false,
        runToken: null
      }),
      activeRun: null,
      groupId: null,
      lastTouched: ++this.touchRevision
    };
  }

  private createAndStoreEntry(
    key: ChatRuntimeKey
  ): RuntimeEntry<TConversation, TMessage, TComposer> {
    const entry = this.createEntry(key, {});
    this.entries.set(key, entry);
    return entry;
  }

  private updatedSnapshot(
    previous: ChatRuntimeSnapshot<TConversation, TMessage, TComposer>,
    updated: ChatRuntimeRunStateUpdate<TConversation, TMessage, TComposer>,
    ownership: Pick<
      ChatRuntimeSnapshot<TConversation, TMessage, TComposer>,
      ChatRuntimeRunOwnedField | "runToken"
    >
  ): ChatRuntimeSnapshot<TConversation, TMessage, TComposer> {
    return Object.freeze({
      ...updated,
      key: previous.key,
      revision: previous.revision + 1,
      messages: updated.messages === previous.messages ? previous.messages : [...updated.messages],
      ...ownership
    });
  }

  private requireEntry(
    canonicalKey: ChatRuntimeKey
  ): RuntimeEntry<TConversation, TMessage, TComposer> {
    const entry = this.entries.get(canonicalKey);
    if (!entry) throw new Error(`Unknown chat runtime key: ${canonicalKey}`);
    return entry;
  }

  private touch(entry: RuntimeEntry<TConversation, TMessage, TComposer>): void {
    entry.lastTouched = ++this.touchRevision;
  }

  private evictInactiveCompletedEntries(protectedKey?: ChatRuntimeKey): boolean {
    const completedInactiveEntries = Array.from(this.entries.entries())
      .filter(
        ([key, entry]) =>
          key !== this.activeKey && entry.activeRun === null && this.canEvict(entry.snapshot)
      )
      .sort((left, right) => left[1].lastTouched - right[1].lastTouched);

    const excess = completedInactiveEntries.length - this.maxInactiveCompletedEntries;
    if (excess <= 0) return false;
    const candidates = completedInactiveEntries.filter(([key]) => key !== protectedKey);
    const entriesToEvict = candidates.slice(0, excess);
    this.runAll(
      entriesToEvict.map(
        ([key]) =>
          () =>
            this.removeEntry(key, "evicted")
      )
    );
    return entriesToEvict.length > 0;
  }

  private removeEntry(canonicalKey: ChatRuntimeKey, reason: "evicted" | "deleted"): void {
    const entry = this.detachEntry(canonicalKey, reason === "evicted");
    if (entry) this.disposeEntry?.(entry.snapshot, reason);
  }

  private detachEntry(
    canonicalKey: ChatRuntimeKey,
    preserveAliases = false
  ): RuntimeEntry<TConversation, TMessage, TComposer> | undefined {
    const entry = this.entries.get(canonicalKey);
    this.entries.delete(canonicalKey);
    // A draft history entry may be the only browser address for a conversation
    // that finished creating offscreen. Keep that lightweight redirect when its
    // cached snapshot is merely evicted, so Back can recreate/load the server
    // conversation. Explicit deletion and account disposal still clear aliases.
    if (preserveAliases) return entry;
    this.detachAliases(canonicalKey);
    return entry;
  }

  private detachAliases(canonicalKey: ChatRuntimeKey): boolean {
    let removed = false;
    for (const alias of Array.from(this.aliases.keys())) {
      if (this.resolveKey(alias) === canonicalKey) {
        this.aliases.delete(alias);
        removed = true;
      }
    }
    return removed;
  }

  private forgetDraftKeysResolvingTo(canonicalKey: ChatRuntimeKey): boolean {
    let changed = false;
    for (const [scopeId, draftKey] of this.rememberedDraftKeys) {
      if (this.resolveKey(draftKey) !== canonicalKey) continue;
      this.rememberedDraftKeys.delete(scopeId);
      changed = true;
    }
    return changed;
  }

  private assertNotDisposed(): void {
    if (this.disposed) throw new Error("Chat runtime store has been disposed");
  }

  private runAll(actions: ReadonlyArray<() => void>): void {
    let firstError: unknown;
    let hasError = false;
    for (const action of actions) {
      try {
        action();
      } catch (error) {
        if (!hasError) {
          firstError = error;
          hasError = true;
        }
      }
    }
    if (hasError) throw firstError;
  }

  private publish(forceActiveRunKeys = false, forceCompletedUnreadKeys = false): void {
    this.publishActiveRunKeys(forceActiveRunKeys);
    this.publishCompletedUnreadKeys(forceCompletedUnreadKeys);
    this.subscriberRevision += 1;
    this.notifyListeners(this.listeners);
  }

  private publishActiveRunKeys(force = false): void {
    const nextKeys = Array.from(this.entries.entries())
      .filter(([, entry]) => entry.activeRun !== null)
      .map(([key]) => key);
    const previousKeys = this.activeRunKeysSnapshot;

    if (
      !force &&
      nextKeys.length === previousKeys.length &&
      nextKeys.every((key, index) => key === previousKeys[index])
    ) {
      return;
    }

    this.activeRunKeysSnapshot =
      nextKeys.length === 0 ? EMPTY_CHAT_RUNTIME_KEYS : Object.freeze(nextKeys);
    this.notifyListeners(this.activeRunListeners);
  }

  private publishCompletedUnreadKeys(force = false): void {
    const nextKeys = Array.from(this.completedUnreadGroups.keys());
    const previousKeys = this.completedUnreadKeysSnapshot;

    if (
      !force &&
      nextKeys.length === previousKeys.length &&
      nextKeys.every((key, index) => key === previousKeys[index])
    ) {
      return;
    }

    this.completedUnreadKeysSnapshot =
      nextKeys.length === 0 ? EMPTY_CHAT_RUNTIME_KEYS : Object.freeze(nextKeys);
    this.notifyListeners(this.completedUnreadListeners);
  }

  private notifyListeners(listeners: Iterable<() => void>): void {
    // Subscribers are observers: one broken observer must not change a completed
    // runtime mutation, hide cancellation metadata, or strand a controller.
    for (const listener of Array.from(listeners)) {
      try {
        listener();
      } catch {
        // Keep notifying the remaining subscribers. Rendering layers surface
        // their own errors without changing this store's ownership outcome.
      }
    }
  }
}
