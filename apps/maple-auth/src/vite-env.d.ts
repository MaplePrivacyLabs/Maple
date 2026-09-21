/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_OPEN_SECRET_API_URL: string;
  readonly VITE_CLIENT_ID?: string;
  readonly VITE_OPEN_SECRET_PCR_ENVIRONMENT?: "production" | "development";
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
