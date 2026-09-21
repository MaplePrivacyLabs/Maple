import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";
import derPlugin from "./vite-der-plugin";
import { assertAuthBundleIsolation } from "./auth-build-boundary";

export default defineConfig({
  envDir: process.env.MAPLE_IGNORE_VITE_ENV_FILES === "1" ? false : undefined,
  plugins: [
    react(),
    derPlugin(),
    {
      name: "auth-bundle-boundary",
      generateBundle() {
        assertAuthBundleIsolation(this.getModuleIds(), __dirname);
      }
    }
  ],
  resolve: {
    alias: { "@": path.resolve(__dirname, "src") },
    dedupe: ["react", "react-dom"]
  },
  build: { outDir: "dist", emptyOutDir: true },
  server: { host: "127.0.0.1", port: 5174, strictPort: true }
});
