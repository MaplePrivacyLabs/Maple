import { createHash } from "node:crypto";
import {
  GITHUB_REPOSITORY,
  INSTALLER_KEYS,
  STABLE_VERSION,
  isInstallerCatalog,
  isInstallerName,
  isRecord,
  type InstallerCatalog,
  type InstallerKey,
} from "./installers";
import { isLatestRelease } from "./worker";

interface ReleaseAsset {
  id: number;
  name: string;
  size: number;
  digest: string;
  state: "uploaded";
  browser_download_url: string;
}

function releaseVersion(release: unknown): string {
  if (
    !isRecord(release) ||
    release.draft !== false ||
    release.prerelease !== false ||
    typeof release.id !== "number" ||
    !Number.isSafeInteger(release.id) ||
    release.id <= 0 ||
    typeof release.tag_name !== "string" ||
    !release.tag_name.startsWith("v") ||
    !STABLE_VERSION.test(release.tag_name.slice(1)) ||
    !Array.isArray(release.assets)
  ) {
    throw new Error("Expected a published stable Research release");
  }
  return release.tag_name.slice(1);
}

function selectAsset(
  release: Record<string, unknown>,
  matches: (name: string) => boolean,
): ReleaseAsset {
  const assets = (release.assets as unknown[]).filter(
    (asset) =>
      isRecord(asset) && typeof asset.name === "string" && matches(asset.name),
  );
  if (assets.length !== 1) {
    throw new Error(
      `Expected exactly one matching release asset, found ${assets.length}`,
    );
  }
  const asset = assets[0];
  if (
    !isRecord(asset) ||
    typeof asset.id !== "number" ||
    !Number.isSafeInteger(asset.id) ||
    asset.id <= 0 ||
    typeof asset.size !== "number" ||
    !Number.isSafeInteger(asset.size) ||
    asset.size <= 0 ||
    asset.state !== "uploaded" ||
    typeof asset.digest !== "string" ||
    !/^sha256:[0-9a-f]{64}$/.test(asset.digest) ||
    typeof asset.browser_download_url !== "string"
  ) {
    throw new Error(
      "Release asset must be uploaded with a valid size and SHA-256 digest",
    );
  }
  return asset as unknown as ReleaseAsset;
}

export function assertSuccessfulReleaseRun(
  release: unknown,
  runs: unknown,
  commit: string,
): void {
  const version = releaseVersion(release);
  if (
    !/^[0-9a-f]{40}$/.test(commit) ||
    !isRecord(runs) ||
    !Array.isArray(runs.workflow_runs)
  ) {
    throw new Error("Missing resolved release commit or workflow runs");
  }
  const successful = runs.workflow_runs.some(
    (run) =>
      isRecord(run) &&
      run.path === ".github/workflows/release.yml" &&
      run.event === "release" &&
      run.status === "completed" &&
      run.conclusion === "success" &&
      run.head_branch === `v${version}` &&
      run.head_sha === commit &&
      isRecord(run.head_repository) &&
      run.head_repository.full_name === GITHUB_REPOSITORY,
  );
  if (!successful) {
    throw new Error(
      "No completed successful Release run matches this repository, tag and commit",
    );
  }
}

export function prepareInstallerCatalog(
  release: unknown,
  latestBytes: Uint8Array,
): InstallerCatalog {
  const version = releaseVersion(release);
  const inventory = release as Record<string, unknown>;
  const latestAsset = selectAsset(inventory, (name) => name === "latest.json");
  if (
    latestAsset.size > 65536 ||
    latestAsset.size !== latestBytes.byteLength ||
    latestAsset.digest !==
      `sha256:${createHash("sha256").update(latestBytes).digest("hex")}` ||
    latestAsset.browser_download_url !==
      `https://github.com/${GITHUB_REPOSITORY}/releases/download/v${version}/latest.json`
  ) {
    throw new Error(
      "Downloaded latest.json does not match the release asset size, digest or URL",
    );
  }
  const latest: unknown = JSON.parse(
    new TextDecoder("utf-8", { fatal: true }).decode(latestBytes),
  );
  if (!isLatestRelease(latest) || latest.version !== version) {
    throw new Error("Invalid or mismatched Tauri updater metadata");
  }
  const installers = {} as InstallerCatalog["installers"];
  for (const key of INSTALLER_KEYS) {
    const asset = selectAsset(inventory, (name) =>
      isInstallerName(key, name, version),
    );
    installers[key] = {
      asset_id: asset.id,
      name: asset.name,
      size: asset.size,
      sha256: asset.digest.slice("sha256:".length),
      url: asset.browser_download_url,
    };
  }
  const catalog: InstallerCatalog = {
    schema_version: 1,
    product: "research",
    channel: "stable",
    version,
    release_id: inventory.id as number,
    installers,
  };
  if (!isInstallerCatalog(catalog))
    throw new Error("Invalid installer catalog");
  const updaterPlatforms: Partial<Record<InstallerKey, string>> = {
    windows: "windows-x86_64",
    "linux-appimage": "linux-x86_64-appimage",
    "linux-deb": "linux-x86_64-deb",
    "linux-rpm": "linux-x86_64-rpm",
  };
  for (const [key, platform] of Object.entries(updaterPlatforms)) {
    if (
      catalog.installers[key as InstallerKey].url !==
      latest.platforms[platform].url
    ) {
      throw new Error(`Installer ${key} does not match Tauri updater metadata`);
    }
  }
  return catalog;
}

export function assertReleaseUnchanged(
  originalRelease: unknown,
  currentRelease: unknown,
  latestBytes: Uint8Array,
  catalog: InstallerCatalog,
  runs: unknown,
  commit: string,
): void {
  assertSuccessfulReleaseRun(currentRelease, runs, commit);
  const currentCatalog = prepareInstallerCatalog(currentRelease, latestBytes);
  if (JSON.stringify(currentCatalog) !== JSON.stringify(catalog)) {
    throw new Error(
      "Release or installer asset identity changed during publication",
    );
  }
  releaseVersion(originalRelease);
  const originalLatest = selectAsset(
    originalRelease as Record<string, unknown>,
    (name) => name === "latest.json",
  );
  const currentLatest = selectAsset(
    currentRelease as Record<string, unknown>,
    (name) => name === "latest.json",
  );
  for (const field of [
    "id",
    "name",
    "size",
    "digest",
    "state",
    "browser_download_url",
  ] as const) {
    if (originalLatest[field] !== currentLatest[field]) {
      throw new Error(
        "The latest.json release asset identity changed during publication",
      );
    }
  }
}

async function headWithRetry(
  url: string,
  fetcher: typeof fetch,
): Promise<Response> {
  for (let attempt = 0; ; attempt++) {
    try {
      const response = await fetcher(url, {
        headers: { "user-agent": "Maple-release-verification/1.0" },
        method: "HEAD",
        redirect: "follow",
        signal: AbortSignal.timeout(30000),
      });
      if (attempt === 2 || (response.status !== 429 && response.status < 500))
        return response;
    } catch (error) {
      if (attempt === 2) throw error;
    }
    await new Promise((resolve) => setTimeout(resolve, 1000 * (attempt + 1)));
  }
}

export async function assertInstallerAvailability(
  catalog: InstallerCatalog,
  fetcher: typeof fetch = fetch,
): Promise<void> {
  // HEAD follows GitHub's temporary asset redirect without downloading installers.
  // Only network failures, throttling and server errors receive bounded retries.
  await Promise.all(
    INSTALLER_KEYS.map(async (key) => {
      const asset = catalog.installers[key];
      const response = await headWithRetry(asset.url, fetcher);
      if (
        response.status !== 200 ||
        response.headers.get("content-length") !== String(asset.size) ||
        !response.headers.get("content-disposition")?.startsWith("attachment;")
      ) {
        throw new Error(
          `Installer ${key} is not available as the expected attachment`,
        );
      }
    }),
  );
}

export async function assertPublishedBundle(
  origin: string,
  catalog: InstallerCatalog,
  latestBytes: Uint8Array,
  fetcher: typeof fetch = fetch,
): Promise<void> {
  const latest = await fetcher(`${origin}/latest.json`, {
    redirect: "manual",
    headers: {
      "cache-control": "no-cache",
      "user-agent": "Maple-release-verification/1.0",
    },
    signal: AbortSignal.timeout(20000),
  });
  if (
    latest.status !== 200 ||
    !Buffer.from(await latest.arrayBuffer()).equals(Buffer.from(latestBytes))
  ) {
    throw new Error(
      "Public latest.json does not match the original release bytes",
    );
  }
  await Promise.all(
    INSTALLER_KEYS.flatMap((key) =>
      ["GET", "HEAD"].map(async (method) => {
        const response = await fetcher(
          `${origin}/download/research/stable/${key}`,
          {
            method,
            redirect: "manual",
            headers: {
              "cache-control": "no-cache",
              "user-agent": "Maple-release-verification/1.0",
            },
            signal: AbortSignal.timeout(20000),
          },
        );
        if (
          response.status !== 302 ||
          response.headers.get("location") !== catalog.installers[key].url ||
          response.headers.get("cache-control") !== "no-store" ||
          response.headers.get("cdn-cache-control") !== "no-store"
        ) {
          throw new Error(
            `Public ${method} download route ${key} does not match the deployed catalog`,
          );
        }
        await response.body?.cancel();
      }),
    ),
  );
}

async function readCatalog(path: string): Promise<InstallerCatalog> {
  const value: unknown = await Bun.file(path).json();
  if (!isInstallerCatalog(value)) throw new Error("Invalid installer catalog");
  return value;
}

if (import.meta.main) {
  const [command, ...args] = Bun.argv.slice(2);
  if (command === "prepare" && args.length === 5) {
    const [releasePath, latestPath, runsPath, commit, output] = args;
    const release: unknown = await Bun.file(releasePath).json();
    assertSuccessfulReleaseRun(
      release,
      await Bun.file(runsPath).json(),
      commit,
    );
    const catalog = prepareInstallerCatalog(
      release,
      new Uint8Array(await Bun.file(latestPath).arrayBuffer()),
    );
    await assertInstallerAvailability(catalog);
    await Bun.write(output, `${JSON.stringify(catalog, null, 2)}\n`);
  } else if (command === "recheck" && args.length === 6) {
    const [
      originalPath,
      currentPath,
      latestPath,
      catalogPath,
      runsPath,
      commit,
    ] = args;
    assertReleaseUnchanged(
      await Bun.file(originalPath).json(),
      await Bun.file(currentPath).json(),
      new Uint8Array(await Bun.file(latestPath).arrayBuffer()),
      await readCatalog(catalogPath),
      await Bun.file(runsPath).json(),
      commit,
    );
  } else if (command === "verify-live" && args.length === 3) {
    const [origin, catalogPath, latestPath] = args;
    await assertPublishedBundle(
      origin,
      await readCatalog(catalogPath),
      new Uint8Array(await Bun.file(latestPath).arrayBuffer()),
    );
  } else {
    throw new Error(
      "Usage: prepare-installers.ts prepare <release.json> <latest.json> <runs.json> <commit> <output.json> | recheck <original.json> <current.json> <latest.json> <catalog.json> <runs.json> <commit> | verify-live <origin> <catalog.json> <latest.json>",
    );
  }
}
