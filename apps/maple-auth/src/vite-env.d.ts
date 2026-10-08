/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_AUTH_ENVIRONMENT: "production" | "development";
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
