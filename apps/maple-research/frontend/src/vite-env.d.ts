/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_OPEN_SECRET_API_URL: string;
  readonly VITE_OPEN_SECRET_PCR_ENVIRONMENT?: "production" | "development";
  readonly VITE_OS_FLAGS_BASE_URL?: string;
  readonly VITE_FORCE_FEATURE_FLAGS?: string;
  readonly VITE_CLIENT_ID?: string;
  readonly VITE_MAPLE_BILLING_API_URL?: string;
  readonly VITE_DEV_MODEL_OVERRIDE?: string;
  readonly VITE_APP_ORIGIN?: string;
  readonly VITE_MARKETING_ORIGIN?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
