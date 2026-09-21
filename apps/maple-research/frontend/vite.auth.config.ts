import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "path";
import derPlugin from "./vite-der-plugin";
import { assertAuthBundleIsolation } from "./auth-build-boundary";

const ignoreEnvFiles = process.env.MAPLE_IGNORE_VITE_ENV_FILES === "1";

export default defineConfig({
  envDir: ignoreEnvFiles ? false : undefined,
  publicDir: "public-auth",
  plugins: [
    react(),
    derPlugin(),
    {
      name: "auth-entry-html",
      generateBundle: {
        order: "post",
        handler(_options, bundle) {
          assertAuthBundleIsolation(this.getModuleIds(), path.resolve(__dirname, "src"));
          const entry = bundle["auth.html"];
          if (entry?.type !== "asset") this.error("The dedicated auth HTML entry was not emitted");
          delete bundle["auth.html"];
          entry.fileName = "index.html";
          bundle["index.html"] = entry;
        }
      },
      configureServer(server) {
        server.middlewares.use((request, _response, next) => {
          // Vite's development fallback would otherwise serve the full-app index.html.
          if (request.headers.accept?.includes("text/html")) request.url = "/auth.html";
          next();
        });
      }
    }
  ],
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
    dedupe: ["react", "react-dom"]
  },
  build: {
    outDir: "dist-auth",
    emptyOutDir: true,
    rollupOptions: { input: path.resolve(__dirname, "auth.html") }
  },
  server: { host: "127.0.0.1", port: 5174, strictPort: true }
});
