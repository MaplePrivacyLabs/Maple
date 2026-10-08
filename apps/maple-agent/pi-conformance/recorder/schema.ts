import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import type { TSchema } from "typebox";
import { Compile } from "typebox/compile";

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export interface Clock { epochMs: number }
export interface FauxResponse {
  content: string | Record<string, Json> | Record<string, Json>[];
  stopReason: "stop" | "length" | "toolUse" | "error" | "aborted";
  errorMessage?: string;
  responseId?: string;
  timestamp?: number;
}
export interface Scenario {
  dsl: 1;
  id: string;
  layer: "agent" | "session" | "wire" | "session+wire";
  covers: string[];
  model: { ref: string };
  clock: Clock;
  systemPrompt?: string;
  thinkingLevel?: "off" | "minimal" | "low" | "medium" | "high" | "xhigh";
  provider: { kind: "faux"; tokenSize: number; tokensPerSecond?: number; responses: FauxResponse[] }
    | { kind: "wire"; responses: Record<string, Json>[] };
  tools?: Record<string, Json>[];
  steeringMode?: "one-at-a-time" | "all";
  followUpMode?: "one-at-a-time" | "all";
  concurrency?: "gated" | "free";
  platforms?: ("linux" | "macos" | "windows")[];
  normalize?: "timestamps"[];
  variants?: { id: string; subscriberDelayOn: string }[];
  steps: Record<string, Json>[];
}
export interface FunctionMatrix {
  dsl: 1;
  id: string;
  clock: Clock;
  cases: { case: string; input: Record<string, Json> }[];
}
export interface ModelMatrix {
  dsl: 1;
  id: string;
  clock: Clock;
  covers: string[];
  models: { provider: string; id: string }[];
}
export interface Input<T> { source: string; relativePath: string; value: T }
export type Validator = ReturnType<typeof Compile>;

export async function loadSchema(root: string): Promise<Validator> {
  const schema = JSON.parse(await readFile(path.join(root, "schema.json"), "utf8")) as TSchema;
  return Compile(schema);
}

export function validateInput(validator: Validator, value: unknown, file: string): asserts value is Scenario | FunctionMatrix | ModelMatrix {
  if (!validator.Check(value)) {
    // Errors contain schema locations and JSON paths, never host paths or secrets.
    const errors = [...validator.Errors(value)].map((error) => `${error.instancePath || "/"}: ${error.message}`);
    throw new Error(`Invalid DSL input ${file}: ${errors.join("; ")}`);
  }
  const input = value as Scenario | FunctionMatrix | ModelMatrix;
  if ("cases" in input) {
    const names = new Set<string>();
    for (const item of input.cases) {
      if (names.has(item.case)) throw new Error(`Duplicate function case ${input.id}: ${item.case}`);
      names.add(item.case);
    }
  } else if ("models" in input) {
    const pairs = new Set<string>();
    for (const model of input.models) {
      const key = `${model.provider}/${model.id}`;
      if (pairs.has(key)) throw new Error(`Duplicate model reference ${input.id}: ${key}`);
      pairs.add(key);
    }
  } else {
    const namespace = input.id.split("/")[0];
    const expected = input.layer === "session+wire" ? "wire" : input.layer;
    if (namespace !== expected) throw new Error(`Scenario ${input.id} does not match layer ${input.layer}`);
  }
}

async function jsonFiles(root: string, directory: string): Promise<string[]> {
  let entries;
  try { entries = await readdir(path.join(root, directory), { withFileTypes: true }); }
  catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return [];
    throw error;
  }
  const files: string[] = [];
  for (const entry of entries.sort((left, right) => left.name < right.name ? -1 : left.name > right.name ? 1 : 0)) {
    const relative = path.posix.join(directory, entry.name);
    if (entry.isSymbolicLink()) throw new Error(`Scenario inputs must not be symlinks: ${relative}`);
    if (entry.isDirectory()) files.push(...await jsonFiles(root, relative));
    else if (entry.isFile() && entry.name.endsWith(".json")) files.push(relative);
  }
  return files;
}

export async function loadInputs(root: string) {
  const validator = await loadSchema(root);
  const scenarios: Input<Scenario>[] = [];
  const functions: Input<FunctionMatrix>[] = [];
  const models: Input<ModelMatrix>[] = [];
  const ids = new Set<string>();
  for (const directory of ["agent", "session", "wire", "functions", "models"]) {
    for (const relativePath of await jsonFiles(root, directory)) {
      const source = await readFile(path.join(root, relativePath), "utf8");
      const value: unknown = JSON.parse(source);
      validateInput(validator, value, relativePath);
      const isFunction = "cases" in value;
      const isModel = "models" in value;
      const expectedPath = isFunction ? `functions/${value.id}.json` : isModel ? `models/${value.id}.json` : `${value.id}.json`;
      if (relativePath !== expectedPath) throw new Error(`DSL id and path disagree: ${relativePath}; expected ${expectedPath}`);
      if (ids.has(value.id)) throw new Error(`Duplicate DSL input id: ${value.id}`);
      ids.add(value.id);
      if (isFunction) functions.push({ source, relativePath, value });
      else if (isModel) models.push({ source, relativePath, value });
      else scenarios.push({ source, relativePath, value });
    }
  }
  if (scenarios.length === 0) throw new Error("No executable scenario inputs found");
  return { validator, scenarios, functions, models };
}
