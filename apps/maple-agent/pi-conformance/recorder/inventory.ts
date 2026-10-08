import { createHash } from "node:crypto";
import { existsSync, statSync } from "node:fs";
import { readFile, readdir, mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { createVitest, experimental_getRunnerTask } from "vitest/node";

type TestRecord = {
  id: string;
  file: string;
  ancestors: string[];
  title: string;
  line: number;
  column: number;
  mode: string;
  status: string;
};
type FileRecord = { path: string; sha256: string };
type Inventory = {
  schemaVersion: 1;
  pin: { tag: string; rev: string };
  files: FileRecord[];
  tests: TestRecord[];
};
type Selection = { path: string; classification: string; note: string };

const root = process.env.PI_WORKSPACE_ROOT;
const selectionRoot = process.env.PI_SELECTION_DIR;
const pinFile = process.env.PI_PIN_JSON;
if (!root || !selectionRoot || !pinFile) throw new Error("Reference roots and pin are required");
const pin = JSON.parse(await readFile(pinFile, "utf8"));
const selection: Selection[] = JSON.parse(
  await readFile(path.join(selectionRoot, "test-files.json"), "utf8"),
).files;
const phaseTwoFiles = new Set([
  "packages/ai/test/openai-completions-retry.test.ts",
  "packages/coding-agent/test/suite/agent-session-mcp.test.ts",
]);
const packages = ["agent", "ai", "coding-agent"];
const sha256 = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");
const compare = (a: string, b: string) => a < b ? -1 : a > b ? 1 : 0;

function stableText(value: string): string {
  // Parameterized test titles sometimes contain a checkout URL or temp path.
  // Only exact environment roots are rewritten; the test's remaining text stays intact.
  return value
    .replaceAll(pathToFileURL(root!).href, "<upstream>")
    .replaceAll(root!, "<upstream>")
    .replaceAll(process.env.HOME!, "<home>")
    .replaceAll(process.env.TMPDIR!, "<tmp>");
}

function relativeFile(value: string): string {
  const relative = path.relative(root!, value).split(path.sep).join("/");
  if (!relative.startsWith("packages/") || relative.includes("../")) {
    throw new Error(`Test module is outside the pinned source: ${value}`);
  }
  return relative;
}

async function discover(directory: string): Promise<string[]> {
  const found: string[] = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const item = path.join(directory, entry.name);
    if (entry.isDirectory()) found.push(...await discover(item));
    else if (/\.(test|spec)\.ts$/.test(entry.name)) found.push(relativeFile(item));
  }
  return found.sort(compare);
}

async function writeJson(filename: string, value: unknown): Promise<void> {
  await mkdir(path.dirname(filename), { recursive: true });
  await writeFile(filename, JSON.stringify(value, null, 2) + "\n");
}

function makeRecords(modules: any[]): TestRecord[] {
  const records: TestRecord[] = [];
  for (const module of [...modules].sort((a, b) => compare(a.moduleId, b.moduleId))) {
    const file = relativeFile(module.moduleId);
    const duplicates = new Map<string, number>();
    for (const test of module.children.allTests()) {
      const ancestors: string[] = [];
      for (let parent = test.parent; parent.type !== "module"; parent = parent.parent) {
        ancestors.unshift(stableText(parent.name));
      }
      const title = stableText(test.name);
      const base = [file, ...ancestors, title].join(" > ");
      const occurrence = (duplicates.get(base) ?? 0) + 1;
      duplicates.set(base, occurrence);
      if (!test.location) throw new Error(`Missing test location: ${base}`);
      records.push({
        id: occurrence === 1 ? base : `${base} #${occurrence}`,
        file, ancestors, title,
        line: test.location.line,
        column: test.location.column,
        mode: experimental_getRunnerTask(test).mode,
        status: test.result().state,
      });
    }
  }
  return records.sort((a, b) => compare(a.id, b.id));
}

async function collectOrRun(mode: string, packageName: string, output: string): Promise<void> {
  if (!packages.includes(packageName)) throw new Error(`Unknown package: ${packageName}`);
  const packageRoot = path.join(root!, "packages", packageName);
  // Match each package's own npm test invocation, including fixtures that use cwd.
  process.chdir(packageRoot);
  const files = mode === "collect"
    ? await discover(path.join(packageRoot, "test"))
    : selection.filter((entry) => entry.path.startsWith(`packages/${packageName}/`)
      && entry.classification !== "E" && !phaseTwoFiles.has(entry.path)).map((entry) => entry.path).sort(compare);
  if (!files.length) throw new Error(`No files selected for ${packageName}`);
  const upstreamConfig = (await import(pathToFileURL(path.join(root!, "vitest.base.ts")).href)).default;
  // The base Vitest aliases omit some experimental workspace packages. Resolve
  // their exact public entry points from the pinned TypeScript configuration so
  // excluded tests can still be collected without building generated dist files.
  const tsconfig = JSON.parse(await readFile(path.join(root!, "tsconfig.json"), "utf8"));
  const sourcePaths = Object.entries(tsconfig.compilerOptions.paths as Record<string, string[]>);
  const sourceAliases = sourcePaths
    .filter(([name]) => name.startsWith("@earendil-works/") && !name.includes("*"))
    .map(([name, targets]) => ({
      find: new RegExp(`^${name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`),
      replacement: path.resolve(root!, targets[0]),
    }));
  const ctx = await createVitest("test", {
    root: packageRoot,
    config: path.join(packageRoot, "vitest.config.ts"),
    watch: false,
    run: true,
    reporters: [],
    includeTaskLocation: true,
    fileParallelism: false,
    maxWorkers: 1,
    include: ["test/**/*.test.ts", "test/**/*.spec.ts"],
  }, {
    resolve: { ...upstreamConfig.resolve, alias: [...upstreamConfig.resolve.alias, ...sourceAliases] },
    plugins: [{
      name: "pi-workspace-source-subpaths",
      enforce: "pre",
      resolveId(specifier) {
        for (const [pattern, targets] of sourcePaths) {
          if (!pattern.startsWith("@earendil-works/") || !pattern.includes("*")) continue;
          const [prefix, suffix] = pattern.split("*");
          if (!specifier.startsWith(prefix) || !specifier.endsWith(suffix)) continue;
          const wildcard = specifier.slice(prefix.length, specifier.length - suffix.length);
          for (const target of targets) {
            const base = path.resolve(root!, target.replace("*", wildcard));
            const candidates = [base, `${base}.ts`, path.join(base, "index.ts")];
            for (const candidate of candidates) {
              if (existsSync(candidate) && statSync(candidate).isFile()) return candidate;
            }
          }
        }
        return null;
      },
    }],
  });
  try {
    const filters = files.map((file) => path.join(root!, file));
    const result = mode === "collect" ? await ctx.collect(filters) : await ctx.start(filters);
    const errors = [...result.unhandledErrors];
    for (const module of result.testModules) {
      errors.push(...module.errors());
      for (const suite of module.children.allSuites()) errors.push(...suite.errors());
      for (const test of module.children.allTests()) errors.push(...(test.result().errors ?? []));
    }
    if (errors.length) throw new AggregateError(errors, `${packageName} ${mode} failed`);
    const collectedFiles = result.testModules.map((module) => relativeFile(module.moduleId)).sort(compare);
    if (JSON.stringify(collectedFiles) !== JSON.stringify(files)) {
      throw new Error(`Incomplete ${packageName} ${mode}: expected ${files.length} files, got ${collectedFiles.length}; missing ${files.filter((file) => !collectedFiles.includes(file)).join(", ")}`);
    }
    const tests = makeRecords(result.testModules);
    if (mode === "run" && tests.some((test) => test.status !== "passed" && test.status !== "skipped")) {
      throw new Error(`Reference tests did not pass: ${tests.filter((test) => test.status !== "passed" && test.status !== "skipped").map((test) => test.id).join("\n")}`);
    }
    const hashes = await Promise.all(files.map(async (file) => ({
      path: file, sha256: sha256(await readFile(path.join(root!, file))),
    })));
    await writeJson(output, { schemaVersion: 1, pin: { tag: pin.tag, rev: pin.rev }, files: hashes, tests });
    console.log(`${packageName} ${mode}: ${files.length} files, ${tests.length} tests (${tests.filter((test) => test.status === "skipped").length} skipped)`);
  } finally {
    await ctx.close();
  }
}

async function combine(directory: string): Promise<void> {
  const inventories: Inventory[] = [];
  const results: Inventory[] = [];
  for (const packageName of packages) {
    inventories.push(JSON.parse(await readFile(path.join(directory, `${packageName}.inventory.json`), "utf8")));
    results.push(JSON.parse(await readFile(path.join(directory, `${packageName}.results.json`), "utf8")));
  }
  const files = inventories.flatMap((inventory) => inventory.files).sort((a, b) => compare(a.path, b.path));
  const tests = inventories.flatMap((inventory) => inventory.tests).sort((a, b) => compare(a.id, b.id));
  const ids = new Set(tests.map((test) => test.id));
  if (ids.size !== tests.length) throw new Error("Duplicate inventory IDs after disambiguation");
  const executed = results.flatMap((result) => result.tests).sort((a, b) => compare(a.id, b.id));
  for (const test of executed) if (!ids.has(test.id)) throw new Error(`Executed test missing from collection: ${test.id}`);
  await writeJson(path.join(directory, "inventory.json"), { schemaVersion: 1, pin: { tag: pin.tag, rev: pin.rev }, files, tests });
  await writeJson(path.join(directory, "results.json"), {
    schemaVersion: 1,
    files: results.flatMap((result) => result.files).sort((a, b) => compare(a.path, b.path)),
    tests: executed.map(({ id, status }) => ({ id, status })),
  });
}

function coverageRecord(test: TestRecord): Record<string, string | number> {
  const selected = selection.find((entry) => entry.path === test.file);
  const record: Record<string, string | number> = {
    id: test.id, file: test.file, line: test.line,
    status: selected && selected.classification !== "E" ? "pending" : "excluded",
  };
  if (!selected) record.reason = "skip-module";
  else if (selected.classification === "E") record.reason = "live-provider";
  if (phaseTwoFiles.has(test.file)) {
    Object.assign(record, { status: "excluded", reason: "replace-module", phase: 2, note: "Host integration is verified in phase 2." });
  }
  if ((test.file.endsWith("/pre-generation-error.test.ts") && test.title === "throws synchronously when auth is missing")
      || (test.file.endsWith("/openai-completions-empty-tools.test.ts") && test.title === "resolves Cloudflare AI Gateway base URL through provider auth")) {
    Object.assign(record, { status: "excluded", reason: "replace-module", note: "Provider authentication and endpoint resolution belong to the host model runtime." });
  }
  if (test.file.endsWith("/openai-completions-empty-tools.test.ts") && /Cloudflare AI Gateway/.test(test.title) && record.status === "pending") {
    record.adaptation = "Keep request-field assertions with a resolved model and explicit authentication/header fixtures.";
  }
  if (test.id === "packages/ai/test/faux-provider.test.ts > faux provider > unregisters the provider") {
    Object.assign(record, { status: "excluded", reason: "replace-module", note: "Provider registry lifecycle is replaced by the host model runtime; retained faux behavior uses the isolated faux handle." });
  }
  return record;
}

async function coverage(input: string, output: string): Promise<void> {
  const inventory: Inventory = JSON.parse(await readFile(input, "utf8"));
  const overrides = JSON.parse(await readFile(path.join(selectionRoot!, "coverage-overrides.json"), "utf8"));
  if (overrides.schemaVersion !== 1 || !Array.isArray(overrides.tests)) throw new Error("Invalid coverage override schema");
  const ids = new Set(inventory.tests.map((test) => test.id));
  const byId = new Map<string, Record<string, string>>();
  const reasons = new Set(["live-provider", "skip-module", "replace-module", "later-module", "ts-runtime", "test-infra", "platform-process", "language"]);
  for (const override of overrides.tests) {
    if (!ids.has(override.id) || byId.has(override.id)) throw new Error(`Unknown or duplicate coverage override: ${override.id}`);
    if (!["pending", "excluded"].includes(override.status)
        || Object.keys(override).some((key) => !["id", "status", "reason", "note", "adaptation"].includes(key))
        || Object.values(override).some((value) => typeof value !== "string" || value.length === 0)
        || (override.status === "excluded" && !reasons.has(override.reason))) throw new Error(`Invalid coverage override: ${override.id}`);
    byId.set(override.id, override);
  }
  const lines = [
    "# Generated initial coverage from the pinned runtime inventory. Update statuses as tests are ported.",
    "[meta]", `pin = ${JSON.stringify(pin.tag)}`, `revision = ${JSON.stringify(pin.rev)}`, "",
  ];
  for (const file of inventory.files) lines.push("[[file]]", `path = ${JSON.stringify(file.path)}`, `sha256 = ${JSON.stringify(file.sha256)}`, "");
  for (const test of inventory.tests) {
    lines.push("[[test]]");
    for (const [key, value] of Object.entries({ ...coverageRecord(test), ...byId.get(test.id) })) lines.push(`${key} = ${JSON.stringify(value)}`);
    lines.push("");
  }
  await mkdir(path.dirname(output), { recursive: true });
  await writeFile(output, lines.join("\n"));
}

const [command, first, second] = process.argv.slice(2);
if (command === "collect" || command === "run") await collectOrRun(command, first, second);
else if (command === "combine") await combine(first);
else if (command === "coverage") await coverage(first, second);
else throw new Error("Usage: inventory.ts collect|run <package> <output> | combine <directory> | coverage <inventory> <map>");
