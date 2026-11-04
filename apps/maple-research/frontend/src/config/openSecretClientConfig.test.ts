import { expect, test } from "bun:test";
import { openSecretClientConfig } from "./openSecretClientConfig";

test("Research config identifies the host as Chat", () => {
  const config = openSecretClientConfig();

  expect(config.clientApp).toBe("chat");
  expect(config.pcrConfig.environment).toBeDefined();
});
