import type { PcrEnvironment } from "@mapleai/sdk";

export function parseOpenSecretPcrEnvironment(value: string | undefined): PcrEnvironment {
  if (value === undefined || value === "production") return "production";
  if (value === "development") return "development";
  throw new Error('VITE_OPEN_SECRET_PCR_ENVIRONMENT must be either "production" or "development"');
}

export function openSecretPcrEnvironment(): PcrEnvironment {
  return parseOpenSecretPcrEnvironment(import.meta.env.VITE_OPEN_SECRET_PCR_ENVIRONMENT);
}
