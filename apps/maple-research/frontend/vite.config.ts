import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "path";
import { TanStackRouterVite } from "@tanstack/router-plugin/vite";
import derPlugin from "./vite-der-plugin";

const ignoreEnvFiles = process.env.MAPLE_IGNORE_VITE_ENV_FILES === "1";

// https://vitejs.dev/config/
export default defineConfig({
  envDir: ignoreEnvFiles ? path.resolve(__dirname, "src-tauri") : undefined,
  plugins: [TanStackRouterVite({ autoCodeSplitting: true }), react(), derPlugin()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src")
    }
  },
  server: {
    // Bind to 0.0.0.0 so the iOS simulator can reach the dev server (dev-only).
    host: "0.0.0.0",
    port: 5173,
    strictPort: true
  }
});
