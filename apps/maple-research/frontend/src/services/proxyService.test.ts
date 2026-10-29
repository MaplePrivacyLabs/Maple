import { describe, expect, it } from "bun:test";

import { manualProxyConfigsMatch, type ProxyConfig } from "./proxyService";

const desiredConfig: ProxyConfig = {
  host: "127.0.0.1",
  port: 37615,
  api_key: "workspace-key",
  enabled: true,
  enable_cors: true,
  backend_url: "http://127.0.0.1:31938",
  auto_start: false
};

describe("manualProxyConfigsMatch", () => {
  it("requires the native process to be running with the requested durable config", () => {
    expect(
      manualProxyConfigsMatch(
        {
          ...desiredConfig,
          host: "127.0.0.1",
          api_key: " workspace-key ",
          backend_url: "http://127.0.0.1:31938/"
        },
        desiredConfig
      )
    ).toBe(true);
    expect(manualProxyConfigsMatch({ ...desiredConfig, auto_start: true }, desiredConfig)).toBe(
      false
    );
    expect(manualProxyConfigsMatch({ ...desiredConfig, port: 8080 }, desiredConfig)).toBe(false);
    expect(
      manualProxyConfigsMatch({ ...desiredConfig, api_key: "another-key" }, desiredConfig)
    ).toBe(false);
    expect(
      manualProxyConfigsMatch(
        { ...desiredConfig, backend_url: "https://enclave.trymaple.ai" },
        desiredConfig
      )
    ).toBe(false);
  });

  it("treats an omitted CORS setting as the secure disabled default", () => {
    expect(
      manualProxyConfigsMatch(
        { ...desiredConfig, enable_cors: undefined },
        { ...desiredConfig, enable_cors: false }
      )
    ).toBe(true);
    expect(
      manualProxyConfigsMatch(
        { ...desiredConfig, enable_cors: undefined },
        { ...desiredConfig, enable_cors: true }
      )
    ).toBe(false);
  });
});
