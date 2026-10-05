import { describe, expect, test } from "bun:test";
import { INSTALLER_KEYS } from "./installers";

import {
  GITHUB_REPOSITORY,
  handleRequest,
  isLatestRelease,
  type Env,
} from "./worker";

function releaseUrl(version: string, name: string): string {
  return `https://github.com/${GITHUB_REPOSITORY}/releases/download/v${version}/${name}`;
}

function validRelease(version = "3.3.8") {
  const appImage = {
    signature: "RWQ-test-appimage-signature",
    url: releaseUrl(version, `Maple_${version}_amd64.AppImage`),
  };

  return {
    notes: `See the release notes for v${version}`,
    platforms: {
      "darwin-aarch64": {
        signature: "RWQ-test-macos-signature",
        url: releaseUrl(version, `Maple_${version}_universal.app.tar.gz`),
      },
      "darwin-x86_64": {
        signature: "RWQ-test-macos-signature",
        url: releaseUrl(version, `Maple_${version}_universal.app.tar.gz`),
      },
      "linux-x86_64": appImage,
      "linux-x86_64-appimage": appImage,
      "linux-x86_64-deb": {
        signature: "RWQ-test-deb-signature",
        url: releaseUrl(version, `Maple_${version}_amd64.deb`),
      },
      "linux-x86_64-rpm": {
        signature: "RWQ-test-rpm-signature",
        url: releaseUrl(version, `Maple-${version}-1.x86_64.rpm`),
      },
      "windows-x86_64": {
        signature: "RWQ-test-windows-signature",
        url: releaseUrl(version, `Maple_${version}_x64-setup.exe`),
      },
    },
    pub_date: "2026-08-24T20:00:00Z",
    version,
  };
}

function envReturning(response: Response, requests: Request[] = []): Env {
  return {
    ASSETS: {
      async fetch(request) {
        requests.push(request);
        return response.clone();
      },
    },
  };
}

describe("latest.json validation", () => {
  test("uses the shared Maple GitHub repository identity", () => {
    expect(GITHUB_REPOSITORY).toBe("MaplePrivacyLabs/Maple");
  });

  test("accepts the Maple release schema", () => {
    expect(isLatestRelease(validRelease())).toBe(true);
  });

  test("rejects mismatched tags and non-GitHub artifact URLs", () => {
    const wrongTag = validRelease();
    wrongTag.platforms["windows-x86_64"].url = releaseUrl(
      "3.3.7",
      "Maple_3.3.7_x64-setup.exe",
    );
    expect(isLatestRelease(wrongTag)).toBe(false);

    const wrongHost = validRelease();
    wrongHost.platforms["windows-x86_64"].url =
      "https://downloads.example.com/Maple_3.3.8_x64-setup.exe";
    expect(isLatestRelease(wrongHost)).toBe(false);

    const wrongOwner = validRelease();
    wrongOwner.platforms["windows-x86_64"].url =
      "https://github.com/SomeoneElse/Maple/releases/download/v3.3.8/Maple_3.3.8_x64-setup.exe";
    expect(isLatestRelease(wrongOwner)).toBe(false);

    const nonDefaultPort = validRelease();
    nonDefaultPort.platforms["windows-x86_64"].url =
      "https://github.com:8443/MaplePrivacyLabs/Maple/releases/download/v3.3.8/Maple_3.3.8_x64-setup.exe";
    expect(isLatestRelease(nonDefaultPort)).toBe(false);
  });

  test("rejects malformed timestamps and invalid extra platforms", () => {
    const dateOnly = validRelease();
    dateOnly.pub_date = "2026-08-24";
    expect(isLatestRelease(dateOnly)).toBe(false);

    const impossibleDate = validRelease();
    impossibleDate.pub_date = "2026-02-30T20:00:00Z";
    expect(isLatestRelease(impossibleDate)).toBe(false);

    const invalidExtraPlatform = validRelease() as ReturnType<
      typeof validRelease
    > & {
      platforms: Record<string, { signature: string; url: string }>;
    };
    invalidExtraPlatform.platforms["future-target"] = {
      signature: "",
      url: "https://downloads.example.com/untrusted",
    };
    expect(isLatestRelease(invalidExtraPlatform)).toBe(false);
  });
});

describe("updates Worker", () => {
  test("returns 404 for unknown paths without reading assets", async () => {
    let assetFetches = 0;
    const response = await handleRequest(
      new Request("https://updates.trymaple.ai/"),
      {
        ASSETS: {
          async fetch() {
            assetFetches += 1;
            return new Response(null, { status: 404 });
          },
        },
      },
    );

    expect(response.status).toBe(404);
    expect(response.headers.get("cache-control")).toBe("no-store");
    expect(assetFetches).toBe(0);
  });

  test("allows only GET and HEAD for latest.json", async () => {
    const response = await handleRequest(
      new Request("https://updates.trymaple.ai/latest.json", {
        method: "POST",
      }),
      envReturning(new Response(null, { status: 404 })),
    );

    expect(response.status).toBe(405);
    expect(response.headers.get("allow")).toBe("GET, HEAD");
  });

  test("returns 404 while latest.json is absent", async () => {
    const response = await handleRequest(
      new Request("https://updates.trymaple.ai/latest.json"),
      envReturning(new Response(null, { status: 404 })),
    );

    expect(response.status).toBe(404);
    expect(response.headers.get("content-type")).toBe(
      "text/plain; charset=utf-8",
    );
  });

  test("serves validated JSON with the public cache contract", async () => {
    const requests: Request[] = [];
    const metadata = JSON.stringify(validRelease());
    const response = await handleRequest(
      new Request("https://updates.trymaple.ai/latest.json", {
        headers: { authorization: "Bearer should-not-be-forwarded" },
      }),
      envReturning(
        new Response(metadata, {
          headers: {
            etag: '"release-etag"',
            "last-modified": "Mon, 24 Aug 2026 20:00:00 GMT",
          },
        }),
        requests,
      ),
    );

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual(validRelease());
    expect(response.headers.get("content-type")).toBe(
      "application/json; charset=utf-8",
    );
    expect(response.headers.get("cache-control")).toBe(
      "public, max-age=0, must-revalidate, no-transform",
    );
    expect(response.headers.get("cdn-cache-control")).toBe("no-store");
    expect(response.headers.get("etag")).toBe('"release-etag"');
    expect(requests).toHaveLength(1);
    expect(requests[0].headers.get("accept")).toBe("application/json");
    expect(requests[0].headers.has("authorization")).toBe(false);
  });

  test("serves HEAD with GET headers and no body", async () => {
    const metadata = JSON.stringify(validRelease());
    const response = await handleRequest(
      new Request("https://updates.trymaple.ai/latest.json", {
        method: "HEAD",
      }),
      envReturning(new Response(metadata)),
    );

    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe(
      "application/json; charset=utf-8",
    );
    expect(await response.text()).toBe("");
  });

  test("preserves updater bytes and headers without loading the installer catalog", async () => {
    const metadata = `${JSON.stringify(validRelease(), null, 2)}\n`;
    for (const method of ["GET", "HEAD"]) {
      const response = await handleRequest(
        new Request("https://updates.trymaple.ai/latest.json?deploy=abc", {
          method,
        }),
        {
          ASSETS: {
            async fetch(request) {
              expect(new URL(request.url).pathname).toBe("/latest.json");
              expect(request.method).toBe("GET");
              return new Response(metadata, {
                headers: {
                  etag: '"unchanged"',
                  "last-modified": "Mon, 24 Aug 2026 20:00:00 GMT",
                },
              });
            },
          },
        },
      );
      expect(response.status).toBe(200);
      expect(await response.text()).toBe(method === "HEAD" ? "" : metadata);
      expect(response.headers.get("etag")).toBe('"unchanged"');
      expect(response.headers.get("last-modified")).toBe(
        "Mon, 24 Aug 2026 20:00:00 GMT",
      );
      expect(response.headers.get("cache-control")).toBe(
        "public, max-age=0, must-revalidate, no-transform",
      );
      expect(response.headers.get("cdn-cache-control")).toBe("no-store");
    }
  });

  test("returns 503 for invalid, oversized, or unavailable metadata", async () => {
    const invalid = await handleRequest(
      new Request("https://updates.trymaple.ai/latest.json"),
      envReturning(new Response("<html>challenge</html>")),
    );
    expect(invalid.status).toBe(503);

    const oversized = await handleRequest(
      new Request("https://updates.trymaple.ai/latest.json"),
      envReturning(
        new Response("{}", {
          headers: { "content-length": String(64 * 1024 + 1) },
        }),
      ),
    );
    expect(oversized.status).toBe(503);

    const unavailable = await handleRequest(
      new Request("https://updates.trymaple.ai/latest.json"),
      envReturning(new Response(null, { status: 503 })),
    );
    expect(unavailable.status).toBe(503);
  });
});

function validCatalog() {
  const version = "3.4.1";
  const names = {
    macos: `Maple_${version}_universal.dmg`,
    windows: `Maple_${version}_x64-setup.exe`,
    "linux-appimage": `Maple_${version}_amd64.AppImage`,
    "linux-deb": `Maple_${version}_amd64.deb`,
    "linux-rpm": `Maple-${version}-1.x86_64.rpm`,
    android: "app-universal-release.apk",
  };
  return {
    schema_version: 1,
    product: "research",
    channel: "stable",
    version,
    release_id: 100,
    installers: Object.fromEntries(
      INSTALLER_KEYS.map((key, index) => [
        key,
        {
          asset_id: index + 1,
          name: names[key],
          size: 1000,
          sha256: "a".repeat(64),
          url: releaseUrl(version, names[key]),
        },
      ]),
    ),
  };
}

describe("Research installer redirects", () => {
  test("every installer supports GET and HEAD with an uncached versioned redirect", async () => {
    const catalog = validCatalog();
    for (const key of INSTALLER_KEYS) {
      for (const method of ["GET", "HEAD"]) {
        const requests: Request[] = [];
        const response = await handleRequest(
          new Request(
            `https://updates.trymaple.ai/download/research/stable/${key}?url=https://evil.example/`,
            { method, headers: { authorization: "Bearer do-not-forward" } },
          ),
          envReturning(new Response(JSON.stringify(catalog)), requests),
        );
        expect(response.status).toBe(302);
        expect(response.headers.get("location")).toBe(
          catalog.installers[key].url,
        );
        expect(response.headers.get("cache-control")).toBe("no-store");
        expect(response.headers.get("cdn-cache-control")).toBe("no-store");
        expect(await response.text()).toBe("");
        expect(requests).toHaveLength(1);
        expect(requests[0].url).toBe(
          "https://updates.trymaple.ai/installers.json",
        );
        expect(requests[0].method).toBe("GET");
        expect(requests[0].headers.has("authorization")).toBe(false);
      }
    }
  });

  test("does not expose metadata, other products, channels or unknown installers", async () => {
    for (const path of [
      "/installers.json",
      "/download/research/stable/unknown",
      "/download/research/stable/macos/",
      "/download/agent/stable/macos",
      "/download/research/dev/macos",
      "/download/research/stable/%6dacos",
    ]) {
      const requests: Request[] = [];
      const response = await handleRequest(
        new Request(`https://updates.trymaple.ai${path}`),
        envReturning(new Response(JSON.stringify(validCatalog())), requests),
      );
      expect(response.status).toBe(404);
      expect(requests).toHaveLength(0);
    }
  });

  test("rejects writes without reading assets", async () => {
    const requests: Request[] = [];
    const response = await handleRequest(
      new Request(
        "https://updates.trymaple.ai/download/research/stable/macos",
        {
          method: "POST",
        },
      ),
      envReturning(new Response(JSON.stringify(validCatalog())), requests),
    );
    expect(response.status).toBe(405);
    expect(response.headers.get("allow")).toBe("GET, HEAD");
    expect(requests).toHaveLength(0);
  });

  test("fails closed for missing, invalid, oversized and unavailable catalog", async () => {
    const wrongTarget = validCatalog();
    wrongTarget.installers.macos.url = "https://evil.example/installer.dmg";
    const incomplete = validCatalog();
    delete incomplete.installers.android;
    const wrongProduct = { ...validCatalog(), product: "agent" };
    const wrongVersion = { ...validCatalog(), version: "3.4.2" };
    for (const asset of [
      new Response(null, { status: 404 }),
      new Response(null, { status: 503 }),
      new Response("<html>challenge</html>"),
      new Response(new Uint8Array([0xff])),
      new Response(JSON.stringify(wrongTarget)),
      new Response(JSON.stringify(incomplete)),
      new Response(JSON.stringify(wrongProduct)),
      new Response(JSON.stringify(wrongVersion)),
      new Response(JSON.stringify(validCatalog()), {
        headers: { "content-length": String(64 * 1024 + 1) },
      }),
      new Response(" ".repeat(64 * 1024 + 1)),
    ]) {
      const response = await handleRequest(
        new Request(
          "https://updates.trymaple.ai/download/research/stable/macos",
        ),
        envReturning(asset),
      );
      expect(response.status).toBe(503);
      expect(response.headers.get("location")).toBeNull();
      expect(response.headers.get("cache-control")).toBe("no-store");
    }
    const response = await handleRequest(
      new Request("https://updates.trymaple.ai/download/research/stable/macos"),
      {
        ASSETS: {
          fetch: async () => {
            throw new Error("unavailable");
          },
        },
      },
    );
    expect(response.status).toBe(503);
  });
});
