import repositoryMetadata from "../../../repo.meta.json";

export const INSTALLER_KEYS = [
  "macos",
  "windows",
  "linux-appimage",
  "linux-deb",
  "linux-rpm",
  "android",
] as const;
export type InstallerKey = (typeof INSTALLER_KEYS)[number];

export interface Installer {
  asset_id: number;
  name: string;
  size: number;
  sha256: string;
  url: string;
}

export interface InstallerCatalog {
  schema_version: 1;
  product: "research";
  channel: "stable";
  version: string;
  release_id: number;
  installers: Record<InstallerKey, Installer>;
}

export const GITHUB_REPOSITORY = `${repositoryMetadata.github.owner}/${repositoryMetadata.github.repository}`;
export const STABLE_VERSION = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function positiveInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0;
}

// Keep these identities explicit. Research's updater archive on macOS is not
// its DMG installer, and Agent/Dev artifacts must never enter this catalog.
export function isInstallerName(
  key: InstallerKey,
  name: string,
  version: string,
): boolean {
  if (!STABLE_VERSION.test(version)) return false;
  switch (key) {
    case "macos":
      return name === `Maple_${version}_universal.dmg`;
    case "windows":
      return name === `Maple_${version}_x64-setup.exe`;
    case "linux-appimage":
      return name === `Maple_${version}_amd64.AppImage`;
    case "linux-deb":
      return name === `Maple_${version}_amd64.deb`;
    case "linux-rpm":
      return new RegExp(
        `^Maple-${version.replaceAll(".", "\\.")}-[1-9]\\d*\\.x86_64\\.rpm$`,
      ).test(name);
    case "android":
      return name === "app-universal-release.apk";
  }
}

export function isInstallerCatalog(value: unknown): value is InstallerCatalog {
  if (
    !isRecord(value) ||
    value.schema_version !== 1 ||
    value.product !== "research" ||
    value.channel !== "stable" ||
    typeof value.version !== "string" ||
    !STABLE_VERSION.test(value.version) ||
    !positiveInteger(value.release_id) ||
    !isRecord(value.installers) ||
    Object.keys(value.installers).length !== INSTALLER_KEYS.length
  ) {
    return false;
  }

  const ids = new Set<number>();
  for (const key of INSTALLER_KEYS) {
    const installer = value.installers[key];
    if (
      !isRecord(installer) ||
      !positiveInteger(installer.asset_id) ||
      ids.has(installer.asset_id) ||
      typeof installer.name !== "string" ||
      !isInstallerName(key, installer.name, value.version) ||
      !positiveInteger(installer.size) ||
      typeof installer.sha256 !== "string" ||
      !/^[0-9a-f]{64}$/.test(installer.sha256) ||
      installer.url !==
        `https://github.com/${GITHUB_REPOSITORY}/releases/download/v${value.version}/${installer.name}`
    ) {
      return false;
    }
    ids.add(installer.asset_id);
  }
  return true;
}
