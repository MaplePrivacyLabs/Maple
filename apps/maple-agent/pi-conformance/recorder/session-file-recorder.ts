import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import type { Input, Json, Scenario } from "./schema.ts";
import { observeSessionFile } from "./session-files.ts";

/** File operations are observed after each completed append, including bytes and read-back. */
export async function recordSessionFiles(input: Input<Scenario>, destination: string) {
  if (input.value.id !== "session/load-migrate-repair" || input.value.layer !== "session") {
    throw new Error("Unsupported session file scenario");
  }
  const cwd = input.value.options?.cwd;
  if (typeof cwd !== "string") throw new Error("Session file scenario requires explicit cwd");
  const files: any[] = [];
  const events: unknown[] = [];
  for (const [seq, step] of input.value.steps.entries()) {
    if (!step.sessionFile || typeof step.sessionFile !== "object" || Array.isArray(step.sessionFile)) {
      throw new Error("Session file step lacks its fixture");
    }
    const observation: any = await observeSessionFile(step.sessionFile as Record<string, Json>, input.value.clock.epochMs, cwd);
    files.push(observation);
    const lastState = [...observation.steps].reverse().find(state => Array.isArray(state.entries));
    events.push({ seq, type: "session_file", entries: lastState?.entries.length ?? 0,
      data: { type: "session_file", ...observation } });
  }
  const out = path.join(destination, "scenarios", input.value.id);
  await mkdir(out, { recursive: true });
  await writeFile(path.join(out, "scenario.json"), input.source);
  await writeFile(path.join(out, "events.jsonl"), events.map(row => JSON.stringify(row)).join("\n") + "\n");
  await writeFile(path.join(out, "requests.jsonl"), "");
  await writeFile(path.join(out, "final.json"), JSON.stringify({ state: { files }, queues: {}, errors: [] }, null, 2) + "\n");
}
