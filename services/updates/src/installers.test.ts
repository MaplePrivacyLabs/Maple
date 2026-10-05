import { describe, expect, test } from "bun:test";
import {
  GITHUB_REPOSITORY,
  INSTALLER_KEYS,
  isInstallerCatalog,
} from "./installers";
import {
  assertInstallerAvailability,
  assertPublishedBundle,
  assertReleaseUnchanged,
  assertSuccessfulReleaseRun,
  prepareInstallerCatalog,
} from "./prepare-installers";

import { COMMIT, ORIGIN, VERSION, digest, fixture, url } from "./test-fixtures";

function mockFetch(
  handler: (url: string, init?: RequestInit) => Response | Promise<Response>,
): typeof fetch {
  return ((input: Parameters<typeof fetch>[0], init?: RequestInit) =>
    Promise.resolve(handler(String(input), init))) as typeof fetch;
}

describe("installer catalog preparation", () => {
  test("selects all six installers, including the actual RPM name, without changing updater bytes", () => {
    const { release, latestBytes } = fixture();
    const original = latestBytes.slice();
    const catalog = prepareInstallerCatalog(release, latestBytes);
    expect(isInstallerCatalog(catalog)).toBe(true);
    expect(Object.keys(catalog.installers)).toEqual([...INSTALLER_KEYS]);
    expect(catalog.installers["linux-rpm"].name).toBe(
      "Maple-3.4.1-1.x86_64.rpm",
    );
    expect(catalog.installers.macos.name).toBe("Maple_3.4.1_universal.dmg");
    expect(catalog.installers.android.url).toBe(
      url("app-universal-release.apk"),
    );
    expect(latestBytes).toEqual(original);
    expect(catalog.release_id).toBe(release.id);
    for (const [index, key] of INSTALLER_KEYS.entries()) {
      expect(catalog.installers[key]).toEqual({
        asset_id: release.assets[index].id,
        name: release.assets[index].name,
        size: release.assets[index].size,
        sha256: release.assets[index].digest.slice(7),
        url: release.assets[index].browser_download_url,
      });
    }
  });

  test.each([
    "latest.json",
    "Maple_3.4.1_universal.dmg",
    "app-universal-release.apk",
  ])("rejects a missing or duplicated required asset: %s", (name) => {
    const { release, latestBytes } = fixture();
    const asset = release.assets.find((value) => value.name === name)!;
    expect(() =>
      prepareInstallerCatalog(
        {
          ...release,
          assets: release.assets.filter((value) => value !== asset),
        },
        latestBytes,
      ),
    ).toThrow();
    expect(() =>
      prepareInstallerCatalog(
        { ...release, assets: [...release.assets, { ...asset, id: 999 }] },
        latestBytes,
      ),
    ).toThrow();
  });

  test("rejects ambiguous RPM revisions instead of arbitrarily selecting one", () => {
    const { release, latestBytes } = fixture();
    release.assets.push({
      ...release.assets[4],
      id: 999,
      name: "Maple-3.4.1-2.x86_64.rpm",
      browser_download_url: url("Maple-3.4.1-2.x86_64.rpm"),
    });
    expect(() => prepareInstallerCatalog(release, latestBytes)).toThrow();
  });

  test.each([
    { size: 0 },
    { size: -1 },
    { size: 1.5 },
    { size: Number.MAX_SAFE_INTEGER + 1 },
    { id: 0 },
    { state: "new" },
    { digest: "" },
    { digest: "sha256:bad" },
    { digest: `sha512:${"b".repeat(64)}` },
    { browser_download_url: "https://evil.example/installer.apk" },
    {
      browser_download_url: url("app-universal-release.apk").replace(
        "MaplePrivacyLabs/Maple",
        "SomeoneElse/Maple",
      ),
    },
    {
      browser_download_url: url("app-universal-release.apk").replace(
        "v3.4.1",
        "v3.4.0",
      ),
    },
  ])("rejects an invalid installer asset: %j", (patch) => {
    const { release, latestBytes } = fixture();
    Object.assign(release.assets[5], patch);
    expect(() => prepareInstallerCatalog(release, latestBytes)).toThrow();
  });

  test.each([
    { draft: true },
    { prerelease: true },
    { id: 0 },
    { tag_name: "v3.4.1-beta.1" },
    { tag_name: "agent-v3.4.1" },
    { tag_name: "v03.4.1" },
  ])("rejects a non-stable Research release: %j", (patch) => {
    const { release, latestBytes } = fixture();
    expect(() =>
      prepareInstallerCatalog({ ...release, ...patch }, latestBytes),
    ).toThrow();
  });

  test("rejects mismatched updater asset bytes, size, digest and origin", () => {
    for (const patch of [
      { size: 1 },
      { size: 65537 },
      { digest: `sha256:${"c".repeat(64)}` },
      { browser_download_url: "https://evil.example/latest.json" },
    ]) {
      const { release, latestAsset, latestBytes } = fixture();
      Object.assign(latestAsset, patch);
      expect(() => prepareInstallerCatalog(release, latestBytes)).toThrow();
    }
    const { release, latestBytes } = fixture();
    latestBytes[0] = 32;
    expect(() => prepareInstallerCatalog(release, latestBytes)).toThrow();
  });

  test("rejects invalid updater metadata even when its asset digest is correct", () => {
    for (const mutate of [
      (latest: ReturnType<typeof fixture>["latest"]) => {
        latest.version = "3.4.0";
      },
      (latest: ReturnType<typeof fixture>["latest"]) => {
        latest.platforms["windows-x86_64"].signature = "";
      },
      (latest: ReturnType<typeof fixture>["latest"]) => {
        latest.platforms["windows-x86_64"].url = url("different.exe");
      },
    ]) {
      const { release, latest, latestAsset } = fixture();
      mutate(latest);
      const bytes = new TextEncoder().encode(JSON.stringify(latest));
      Object.assign(latestAsset, {
        size: bytes.byteLength,
        digest: digest(bytes),
      });
      expect(() => prepareInstallerCatalog(release, bytes)).toThrow();
    }
  });
});

describe("catalog identity boundary", () => {
  test("rejects duplicate asset IDs and wrong product, channel, schema or version", () => {
    const { release, latestBytes } = fixture();
    const catalog = prepareInstallerCatalog(release, latestBytes);
    for (const patch of [
      { product: "agent" },
      { channel: "dev" },
      { schema_version: 2 },
      { version: "3.4.2" },
    ]) {
      expect(isInstallerCatalog({ ...catalog, ...patch })).toBe(false);
    }
    catalog.installers.android.asset_id = catalog.installers.macos.asset_id;
    expect(isInstallerCatalog(catalog)).toBe(false);
    release.assets[5].id = release.assets[0].id;
    expect(() => prepareInstallerCatalog(release, latestBytes)).toThrow();
  });

  test("rejects incomplete, extra and redirect-capable targets", () => {
    const { release, latestBytes } = fixture();
    const catalog = prepareInstallerCatalog(release, latestBytes);
    const { android: _android, ...incomplete } = catalog.installers;
    expect(isInstallerCatalog({ ...catalog, installers: incomplete })).toBe(
      false,
    );
    expect(
      isInstallerCatalog({
        ...catalog,
        installers: {
          ...catalog.installers,
          agent: catalog.installers.android,
        },
      }),
    ).toBe(false);
    for (const target of [
      `${catalog.installers.android.url}?redirect=https://evil.example`,
      `${catalog.installers.android.url}#other`,
      catalog.installers.android.url.replace(
        "https://github.com/",
        "https://github.com@evil.example/",
      ),
    ]) {
      expect(
        isInstallerCatalog({
          ...catalog,
          installers: {
            ...catalog.installers,
            android: { ...catalog.installers.android, url: target },
          },
        }),
      ).toBe(false);
    }
  });
});

describe("release completion and publication identity", () => {
  test("requires a successful run matching the exact release workflow, repository, event, tag and commit", () => {
    const { release, run } = fixture();
    expect(() =>
      assertSuccessfulReleaseRun(release, { workflow_runs: [run] }, COMMIT),
    ).not.toThrow();
    for (const patch of [
      { path: ".github/workflows/agent-desktop-build.yml" },
      { event: "workflow_dispatch" },
      { status: "in_progress" },
      { conclusion: "failure" },
      { conclusion: "cancelled" },
      { head_branch: "v3.4.0" },
      { head_sha: "b".repeat(40) },
      { head_repository: { full_name: "SomeoneElse/Maple" } },
    ]) {
      expect(() =>
        assertSuccessfulReleaseRun(
          release,
          { workflow_runs: [{ ...run, ...patch }] },
          COMMIT,
        ),
      ).toThrow();
    }
    for (const runs of [null, {}, { workflow_runs: [] }]) {
      expect(() => assertSuccessfulReleaseRun(release, runs, COMMIT)).toThrow();
    }
    expect(() =>
      assertSuccessfulReleaseRun(release, { workflow_runs: [run] }, "master"),
    ).toThrow();
  });

  test("rejects changed assets including an identical latest.json reuploaded under a new ID", () => {
    const { release, latestBytes, run } = fixture();
    const catalog = prepareInstallerCatalog(release, latestBytes);
    const runs = { workflow_runs: [run] };
    expect(() =>
      assertReleaseUnchanged(
        release,
        structuredClone(release),
        latestBytes,
        catalog,
        runs,
        COMMIT,
      ),
    ).not.toThrow();
    for (const [index, patch] of [
      [0, { id: 999 }],
      [0, { size: 999 }],
      [0, { digest: `sha256:${"c".repeat(64)}` }],
      [6, { id: 999 }],
    ] as const) {
      const current = structuredClone(release);
      Object.assign(current.assets[index], patch);
      expect(() =>
        assertReleaseUnchanged(
          release,
          current,
          latestBytes,
          catalog,
          runs,
          COMMIT,
        ),
      ).toThrow();
    }
  });
});

describe("installer availability", () => {
  test("checks every asset with HEAD and validates attachment size", async () => {
    const { release, latestBytes } = fixture();
    const catalog = prepareInstallerCatalog(release, latestBytes);
    const requests: string[] = [];
    await assertInstallerAvailability(
      catalog,
      mockFetch((target, init) => {
        requests.push(target);
        expect(init?.method).toBe("HEAD");
        expect(init?.redirect).toBe("follow");
        expect(init?.signal).toBeInstanceOf(AbortSignal);
        const asset = Object.values(catalog.installers).find(
          (value) => value.url === target,
        )!;
        return new Response(null, {
          headers: {
            "content-length": String(asset.size),
            "content-disposition": `attachment; filename=${asset.name}`,
          },
        });
      }),
    );
    expect(requests).toEqual(
      INSTALLER_KEYS.map((key) => catalog.installers[key].url),
    );
  });

  test("rejects HTTP failures, missing/wrong lengths, inline bodies and network failures", async () => {
    const { release, latestBytes } = fixture();
    const catalog = prepareInstallerCatalog(release, latestBytes);
    for (const response of [
      new Response(null, { status: 404 }),
      new Response(null, { status: 403 }),
      new Response(null, {
        headers: { "content-disposition": "attachment; filename=installer" },
      }),
      new Response(null, {
        headers: {
          "content-length": "999",
          "content-disposition": "attachment; filename=installer",
        },
      }),
      new Response(null, {
        headers: { "content-length": "1000", "content-disposition": "inline" },
      }),
    ]) {
      await expect(
        assertInstallerAvailability(
          catalog,
          mockFetch(() => response),
        ),
      ).rejects.toThrow();
    }
    await expect(
      assertInstallerAvailability(
        catalog,
        mockFetch(() => {
          throw new Error("network unavailable");
        }),
      ),
    ).rejects.toThrow();
  });
});

describe("published bundle verification", () => {
  test("checks byte-identical updater metadata and every GET/HEAD redirect", async () => {
    const { release, latestBytes } = fixture();
    const catalog = prepareInstallerCatalog(release, latestBytes);
    const requests: string[] = [];
    await assertPublishedBundle(
      ORIGIN,
      catalog,
      latestBytes,
      mockFetch((target, init) => {
        requests.push(`${init?.method ?? "GET"} ${target}`);
        expect(new Headers(init?.headers).get("cache-control")).toBe(
          "no-cache",
        );
        if (target.endsWith("/latest.json")) return new Response(latestBytes);
        expect(init?.redirect).toBe("manual");
        const key = INSTALLER_KEYS.find(
          (key) => target === `${ORIGIN}/download/research/stable/${key}`,
        )!;
        return new Response(null, {
          status: 302,
          headers: {
            location: catalog.installers[key].url,
            "cache-control": "no-store",
            "cdn-cache-control": "no-store",
          },
        });
      }),
    );
    expect(requests).toEqual([
      `GET ${ORIGIN}/latest.json`,
      ...INSTALLER_KEYS.flatMap((key) =>
        ["GET", "HEAD"].map(
          (method) => `${method} ${ORIGIN}/download/research/stable/${key}`,
        ),
      ),
    ]);
  });

  test("rejects different updater bytes, incorrect redirects and cached responses", async () => {
    const { release, latestBytes, latest } = fixture();
    const catalog = prepareInstallerCatalog(release, latestBytes);
    for (const response of [
      new Response(null, { status: 503 }),
      new Response(JSON.stringify(latest)),
    ]) {
      await expect(
        assertPublishedBundle(
          ORIGIN,
          catalog,
          latestBytes,
          mockFetch(() => response),
        ),
      ).rejects.toThrow();
    }
    for (const patch of [
      { status: 200 },
      { status: 301 },
      { location: "https://github.com/MaplePrivacyLabs/Maple/releases/latest" },
      { "cache-control": "public, max-age=300" },
      { "cdn-cache-control": "public, max-age=300" },
    ]) {
      await expect(
        assertPublishedBundle(
          ORIGIN,
          catalog,
          latestBytes,
          mockFetch((target) => {
            if (target.endsWith("/latest.json"))
              return new Response(latestBytes);
            const { status = 302, ...headers } = patch;
            return new Response(null, {
              status,
              headers: {
                location: catalog.installers.macos.url,
                "cache-control": "no-store",
                "cdn-cache-control": "no-store",
                ...headers,
              },
            });
          }),
        ),
      ).rejects.toThrow();
    }
  });
});
