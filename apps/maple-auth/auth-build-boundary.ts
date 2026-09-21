import fs from "node:fs";
import path from "node:path";

function canonicalPath(value: string): string {
  const resolved = path.resolve(value);
  return (fs.existsSync(resolved) ? fs.realpathSync(resolved) : resolved).replace(/\\/gu, "/");
}

/** The auth artifact may contain only this application and its installed dependencies. */
export function assertAuthBundleIsolation(moduleIds: Iterable<string>, appRoot: string): void {
  const prefix = `${canonicalPath(appRoot)}/`;
  for (const moduleId of moduleIds) {
    // Rollup's CommonJS proxy IDs can wrap absolute paths in a virtual prefix.
    const id = moduleId.replace(/^\0/u, "").replace(/\\/gu, "/").split("?")[0];
    if (id.includes("/@opensecret/") || id.includes("/@opensecret+")) {
      throw new Error("The auth build must not include the legacy SDK");
    }
    if (!path.isAbsolute(id)) continue; // Vite and Rollup generated helper modules.
    if (!canonicalPath(id).startsWith(prefix)) {
      throw new Error(`The auth build imported a module outside its application: ${id}`);
    }
  }
}
