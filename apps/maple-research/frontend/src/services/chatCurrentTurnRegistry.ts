type ChatCurrentTurnControl = Readonly<{
  responseRequestStarted: () => boolean;
  serverRequestInFlight?: () => boolean;
  restoreBeforeRequest: (message: string) => boolean;
  retainedPayload?: unknown;
  retainsPayload?: () => boolean;
  countsTowardQueueLimit?: boolean;
}>;

export type RegisteredChatCurrentTurnPayload = Readonly<{
  payload: unknown;
  countsTowardQueueLimit: boolean;
}>;

const currentTurnsByStore = new WeakMap<object, Map<number, ChatCurrentTurnControl>>();

function controlsFor(store: object): Map<number, ChatCurrentTurnControl> {
  const existing = currentTurnsByStore.get(store);
  if (existing) return existing;
  const created = new Map<number, ChatCurrentTurnControl>();
  currentTurnsByStore.set(store, created);
  return created;
}

export function registerChatCurrentTurn(
  store: object,
  runToken: number,
  control: ChatCurrentTurnControl
): () => void {
  const controls = controlsFor(store);
  controls.set(runToken, control);
  return () => {
    if (controls.get(runToken) === control) controls.delete(runToken);
  };
}

/**
 * Restores a detached turn only while it is still entirely client-side. Once a
 * Responses request starts, replaying it would risk a duplicate assistant turn.
 */
export function restoreRegisteredChatTurnBeforeRequest(
  store: object,
  runToken: number,
  message: string
): boolean {
  const control = currentTurnsByStore.get(store)?.get(runToken);
  if (!control || control.responseRequestStarted()) return false;
  return control.restoreBeforeRequest(message);
}

/**
 * Deletion may only retire the run locally when no server mutation is still
 * capable of committing. Stop can abort a conversation-create request, but a
 * destructive caller must keep waiting until that request settles.
 */
export function registeredChatTurnCanSettleLocallyForDeletion(
  store: object,
  runToken: number
): boolean {
  const control = currentTurnsByStore.get(store)?.get(runToken);
  return Boolean(
    control && !control.responseRequestStarted() && !control.serverRequestInFlight?.()
  );
}

export function getRegisteredChatCurrentTurnPayloads(
  store: object
): readonly RegisteredChatCurrentTurnPayload[] {
  const controls = currentTurnsByStore.get(store);
  if (!controls) return [];
  return Array.from(controls.values()).flatMap((control) =>
    control.retainedPayload === undefined || control.retainsPayload?.() === false
      ? []
      : [
          {
            payload: control.retainedPayload,
            countsTowardQueueLimit: control.countsTowardQueueLimit ?? false
          }
        ]
  );
}
