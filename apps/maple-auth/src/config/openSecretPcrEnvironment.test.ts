import { describe, expect, test } from "bun:test";
import { parseOpenSecretPcrEnvironment } from "./openSecretPcrEnvironment";

describe("OpenSecret PCR environment", () => {
  test("defaults to production", () => {
    expect(parseOpenSecretPcrEnvironment(undefined)).toBe("production");
  });

  test("accepts the explicit production and development values", () => {
    expect(parseOpenSecretPcrEnvironment("production")).toBe("production");
    expect(parseOpenSecretPcrEnvironment("development")).toBe("development");
  });

  test("rejects unknown values", () => {
    for (const value of ["", "dev", "prod", "Development", "staging"]) {
      expect(() => parseOpenSecretPcrEnvironment(value)).toThrow(
        /VITE_OPEN_SECRET_PCR_ENVIRONMENT/
      );
    }
  });
});
