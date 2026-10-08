import { existsSync, mkdtempSync, readFileSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { vi } from "vitest";
import { deterministicEnvironment } from "./determinism.ts";
import type { Json } from "./schema.ts";

type Operation = { operation: "appendMessage"; message: object } | { operation: "appendModelChange"; provider: string; modelId: string };
type FileCase = { case: string; mode?: "create"; content?: string; operations: Operation[] };
const snapshot = (value: unknown) => JSON.parse(JSON.stringify(value));
const bytes = (file: string) => existsSync(file) ? readFileSync(file, "utf8") : null;

/** Only this helper-owned temporary filename is normalized in source errors. */
export async function observeSessionFile(input: Record<string, Json>, epochMs: number, cwd: string): Promise<unknown> {
  const item = input as unknown as FileCase;
  const directory = mkdtempSync(path.join(tmpdir(), "session-conformance-"));
  vi.resetModules();
  const clock = deterministicEnvironment(epochMs);
  try {
    const { SessionManager } = await import("../../src/core/session-manager.ts");
    let file = path.join(directory, "session.jsonl");
    if (item.content !== undefined) writeFileSync(file, item.content);
    const steps: unknown[] = [];
    let readBack: unknown;
    const portableError = (error: unknown) => ({
      error: error instanceof Error ? error.message.replaceAll(file, "<SESSION_FILE>") : String(error),
      errorClass: error instanceof Error ? error.name : typeof error,
    });
    try {
      const manager = item.mode === "create"
        ? SessionManager.create(cwd, directory, { id: "barrier-session" })
        : SessionManager.open(file, directory, cwd);
      file = manager.getSessionFile()!;
      const observe = (result?: unknown) => snapshot({ result, bytes: bytes(file), header: manager.getHeader(), entries: manager.getEntries(), leafId: manager.getLeafId(), context: manager.buildSessionContext() });
      steps.push(observe());
      for (const operation of item.operations) {
        const result = operation.operation === "appendMessage"
          ? manager.appendMessage(operation.message as Parameters<typeof manager.appendMessage>[0])
          : manager.appendModelChange(operation.provider, operation.modelId);
        steps.push(observe(result));
      }
      if (existsSync(file)) {
        const reopened = SessionManager.open(file, directory, cwd);
        readBack = snapshot({ header: reopened.getHeader(), entries: reopened.getEntries(), context: reopened.buildSessionContext() });
      } else readBack = null;
    } catch (error) { steps.push(portableError(error)); readBack = portableError(error); }
    let listing: unknown = [];
    if (existsSync(file)) {
      const time = new Date(epochMs);
      utimesSync(file, time, time);
      listing = (await SessionManager.listAll(directory)).map(({ path: _path, ...info }) => info);
    }
    return snapshot({ case: item.case, steps, finalBytes: bytes(file), readBack, listing });
  } finally { clock.restore(); rmSync(directory, { recursive: true, force: true }); }
}
