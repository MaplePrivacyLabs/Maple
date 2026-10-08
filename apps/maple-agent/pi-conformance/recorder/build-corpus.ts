import { createHash } from "node:crypto";
import { copyFile, mkdir, readFile, readdir, stat, writeFile } from "node:fs/promises";
import path from "node:path";

const output = process.argv[2];
const inputs = process.env.PI_HARNESS_INPUTS;
const upstream = process.env.PI_UPSTREAM_RESULTS;
const workspace = process.env.PI_WORKSPACE_ROOT;
if (!output || !inputs || !upstream || !workspace) throw new Error("Corpus, input, and upstream roots are required");
if (process.version !== "v22.23.2") throw new Error(`Unexpected reference Node version: ${process.version}`);
const compare = (a: string, b: string) => a < b ? -1 : a > b ? 1 : 0;
const sha256 = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");

async function filesUnder(root: string, directory = ""): Promise<string[]> {
  const files: string[] = [];
  for (const entry of await readdir(path.join(root, directory), { withFileTypes: true })) {
    const relative = path.posix.join(directory, entry.name);
    if (entry.isDirectory()) files.push(...await filesUnder(root, relative));
    else if (entry.isFile()) files.push(relative);
    else throw new Error(`Corpus inputs must be regular files: ${relative}`);
  }
  return files.sort(compare);
}

await mkdir(path.join(output, "upstream"), { recursive: true });
for (const filename of ["inventory.json", "results.json"]) {
  await copyFile(path.join(upstream, filename), path.join(output, "upstream", filename));
}
const inputNames = ["pin.json", "flake.nix", "flake.lock"];
for (const directory of ["scenarios", "recorder", "nix", "fixtures", "selection"]) {
  const info = await stat(path.join(inputs, directory)).catch((error) => {
    if (error.code === "ENOENT") return undefined;
    throw error;
  });
  if (info?.isDirectory()) inputNames.push(...(await filesUnder(inputs, directory)));
}
const inputHashes: Record<string, string> = {};
for (const filename of inputNames.sort(compare)) {
  inputHashes[filename] = sha256(await readFile(path.join(inputs, filename)));
}
const outputHashes: Record<string, string> = {};
for (const filename of await filesUnder(output)) {
  if (filename === "manifest.json") throw new Error("A recording must start without an existing manifest");
  outputHashes[filename] = sha256(await readFile(path.join(output, filename)));
}
const manifest = {
  schemaVersion: 1,
  pin: JSON.parse(await readFile(path.join(inputs, "pin.json"), "utf8")),
  nodeVersion: process.version,
  upstreamLockfileSha256: sha256(await readFile(path.join(workspace, "package-lock.json"))),
  inputHashes,
  outputHashes,
};
await writeFile(path.join(output, "manifest.json"), JSON.stringify(manifest, null, 2) + "\n");
