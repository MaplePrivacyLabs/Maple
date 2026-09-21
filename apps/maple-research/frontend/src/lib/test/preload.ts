import { mock } from "bun:test";
import { createRequire } from "node:module";
import * as react from "react";
import * as jsxRuntime from "react/jsx-runtime";

// Mirror Vite's React deduplication for a local SDK link. Keep the SDK real:
// only its copy of the React peer dependency is redirected to this renderer's
// instance. Published SDK installs already resolve the same peer and do nothing.
const frontendRequire = createRequire(import.meta.url);
const sdkRequire = createRequire(frontendRequire.resolve("@mapleai/sdk"));
for (const [name, exports] of [
  ["react", react],
  ["react/jsx-runtime", jsxRuntime]
] as const) {
  const sdkPath = sdkRequire.resolve(name);
  if (sdkPath !== frontendRequire.resolve(name)) mock.module(sdkPath, () => exports);
}
