export type AuthEnvironment = "development" | "production";
export type NativeAppVariant = "dev" | undefined;

const CLIENT_ID = "ba5a14b5-d915-47b1-b7b1-afda52bc5fc6";

/** One compiled selector binds backend, PCR trust and native application identity. */
export function parseAuthEnvironment(value: string | undefined): AuthEnvironment {
  if (value === "development" || value === "production") return value;
  throw new Error('VITE_AUTH_ENVIRONMENT must be "development" or "production"');
}

export function authEnvironment(): AuthEnvironment {
  return parseAuthEnvironment(import.meta.env.VITE_AUTH_ENVIRONMENT);
}

export function authConfig(environment: AuthEnvironment = authEnvironment()) {
  return {
    apiUrl:
      environment === "development"
        ? "https://enclave.secretgpt.ai"
        : "https://enclave.trymaple.ai",
    clientId: CLIENT_ID,
    pcrEnvironment: environment
  };
}

export function isNativeAppVariantAllowed(
  variant: unknown,
  environment: AuthEnvironment = authEnvironment()
): variant is NativeAppVariant {
  return environment === "development" ? variant === "dev" : variant === undefined;
}

export function nativeAuthReturnUrl(variant: NativeAppVariant): string {
  if (!isNativeAppVariantAllowed(variant)) {
    throw new Error("Native application does not match this authentication environment");
  }
  return variant === "dev" ? "cloud.opensecret.maple.dev://auth" : "cloud.opensecret.maple://auth";
}
