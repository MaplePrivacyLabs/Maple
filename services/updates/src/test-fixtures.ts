import { createHash } from "node:crypto";
import { GITHUB_REPOSITORY } from "./installers";

export const VERSION = "3.4.1";
export const COMMIT = "a".repeat(40);
export const ORIGIN = "https://updates.trymaple.ai";
export const url = (name: string) =>
  `https://github.com/${GITHUB_REPOSITORY}/releases/download/v${VERSION}/${name}`;
export const digest = (bytes: Uint8Array) =>
  `sha256:${createHash("sha256").update(bytes).digest("hex")}`;

export function fixture() {
  const names = [
    `Maple_${VERSION}_universal.dmg`,
    `Maple_${VERSION}_x64-setup.exe`,
    `Maple_${VERSION}_amd64.AppImage`,
    `Maple_${VERSION}_amd64.deb`,
    `Maple-${VERSION}-1.x86_64.rpm`,
    "app-universal-release.apk",
  ];
  const platform = (name: string) => ({
    signature: "RWQ-fixture-signature",
    url: url(name),
  });
  const latest = {
    version: VERSION,
    pub_date: "2026-09-15T01:29:10Z",
    notes: "Research release",
    platforms: {
      "darwin-aarch64": platform("Maple.app.tar.gz"),
      "darwin-x86_64": platform("Maple.app.tar.gz"),
      "linux-x86_64": platform(names[2]),
      "linux-x86_64-appimage": platform(names[2]),
      "linux-x86_64-deb": platform(names[3]),
      "linux-x86_64-rpm": platform(names[4]),
      "windows-x86_64": platform(names[1]),
    },
  };
  const latestBytes = new TextEncoder().encode(
    `${JSON.stringify(latest, null, 2)}\n`,
  );
  const installers = names.map((name, index) => ({
    id: index + 1,
    name,
    size: 1000 + index,
    digest: `sha256:${"b".repeat(64)}`,
    state: "uploaded",
    browser_download_url: url(name),
  }));
  const latestAsset = {
    id: 100,
    name: "latest.json",
    size: latestBytes.byteLength,
    digest: digest(latestBytes),
    state: "uploaded",
    browser_download_url: url("latest.json"),
  };
  const release = {
    id: 200,
    tag_name: `v${VERSION}`,
    draft: false,
    prerelease: false,
    assets: [...installers, latestAsset],
  };
  const run = {
    id: 300,
    path: ".github/workflows/release.yml",
    event: "release",
    status: "completed",
    conclusion: "success",
    head_branch: `v${VERSION}`,
    head_sha: COMMIT,
    head_repository: { full_name: GITHUB_REPOSITORY },
  };
  return { release, latest, latestBytes, latestAsset, run };
}
