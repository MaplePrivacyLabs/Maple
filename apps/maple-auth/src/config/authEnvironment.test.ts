import { afterEach, describe, expect, test } from "bun:test";
import {
  authConfig,
  isNativeAppVariantAllowed,
  nativeAuthReturnUrl,
  parseAuthEnvironment
} from "./authEnvironment";
import { openSecretClientConfig } from "./openSecretClientConfig";

const originalEnvironment = process.env.VITE_AUTH_ENVIRONMENT;
afterEach(() => {
  if (originalEnvironment === undefined) delete process.env.VITE_AUTH_ENVIRONMENT;
  else process.env.VITE_AUTH_ENVIRONMENT = originalEnvironment;
});

describe("fixed hosted authentication environments", () => {
  test("requires an explicit supported selector", () => {
    for (const value of [undefined, "", "dev", "prod", "Development", "https://other.test"]) {
      expect(() => parseAuthEnvironment(value)).toThrow("VITE_AUTH_ENVIRONMENT");
    }
    expect(parseAuthEnvironment("development")).toBe("development");
    expect(parseAuthEnvironment("production")).toBe("production");
  });

  for (const environment of ["development", "production"] as const) {
    test(`${environment} binds the API, project and PCR environment`, () => {
      process.env.VITE_AUTH_ENVIRONMENT = environment;
      const expectedApi =
        environment === "development"
          ? "https://enclave.secretgpt.ai"
          : "https://enclave.trymaple.ai";
      expect(authConfig(environment)).toEqual({
        apiUrl: expectedApi,
        clientId: "ba5a14b5-d915-47b1-b7b1-afda52bc5fc6",
        pcrEnvironment: environment
      });
      const sdk = openSecretClientConfig();
      expect(sdk.apiUrl).toBe(expectedApi);
      expect(sdk.clientId).toBe("ba5a14b5-d915-47b1-b7b1-afda52bc5fc6");
      expect(sdk.pcrConfig.environment).toBe(environment);
      expect(sdk.pcrConfig.pcr0Values.length).toBeGreaterThan(0);
      expect(sdk.pcrConfig.pcr0DevValues.length).toBeGreaterThan(0);
    });
  }

  test("permits only the fixed native identity for each environment", () => {
    expect(isNativeAppVariantAllowed(undefined, "production")).toBe(true);
    expect(isNativeAppVariantAllowed("dev", "development")).toBe(true);
    expect(isNativeAppVariantAllowed("dev", "production")).toBe(false);
    expect(isNativeAppVariantAllowed(undefined, "development")).toBe(false);
    for (const variant of [
      "prod",
      "Dev",
      "",
      null,
      "cloud.opensecret.maple.dev",
      "https://other.test"
    ]) {
      expect(isNativeAppVariantAllowed(variant, "production")).toBe(false);
      expect(isNativeAppVariantAllowed(variant, "development")).toBe(false);
    }
    process.env.VITE_AUTH_ENVIRONMENT = "development";
    expect(nativeAuthReturnUrl("dev")).toBe("cloud.opensecret.maple.dev://auth");
    expect(() => nativeAuthReturnUrl(undefined)).toThrow("does not match");
    process.env.VITE_AUTH_ENVIRONMENT = "production";
    expect(nativeAuthReturnUrl(undefined)).toBe("cloud.opensecret.maple://auth");
    expect(() => nativeAuthReturnUrl("dev")).toThrow("does not match");
  });
});
