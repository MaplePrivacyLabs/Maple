import { restoreRegisteredChatTurnBeforeRequest } from "./chatCurrentTurnRegistry";
import type { ChatRuntimeKey } from "./chatRuntimeStore";

type CancellableChatRuntimeStore = object & {
  getActiveRunKeys: () => readonly ChatRuntimeKey[];
  get: (key: ChatRuntimeKey) => Readonly<{ runToken: number | null }> | undefined;
  cancelRun: (key: ChatRuntimeKey, token: number) => unknown;
};

/** Synchronously fences every active client runner at an account boundary. */
export function cancelActiveChatRuntimeRuns(store: CancellableChatRuntimeStore): void {
  for (const key of store.getActiveRunKeys()) {
    const token = store.get(key)?.runToken;
    if (token !== null && token !== undefined) {
      restoreRegisteredChatTurnBeforeRequest(
        store,
        token,
        "Sending stopped because this account session is closing."
      );
      store.cancelRun(key, token);
    }
  }
}
