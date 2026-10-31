export const MAX_ENVELOPE_BYTES = 128 * 1024;
export const MAX_MANIFEST_BYTES = 64 * 1024;
export const MAX_SIGNATURE_BYTES = 4096;
export const MAX_ARTIFACT_BYTES = 4 * 1024 ** 3;
export const VERSION = /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/;
const encoder = new TextEncoder();

export interface Artifact {
  path: string;
  size: number;
  sha256: string;
}
interface ManifestFields {
  product: "maple-agent";
  channel: "stable";
  version: string;
  build: string;
  source_sha: string;
  pub_date: string;
  notes: string;
}
export interface Manifest extends ManifestFields {
  format: "maple-agent-update-v1";
  artifacts: { "darwin-aarch64": Artifact; "linux-x86_64"?: Artifact };
  downloads: { macos: Artifact; "linux-appimage"?: Artifact };
}
export interface ManifestV2 extends ManifestFields {
  format: "maple-agent-update-v2";
  artifacts: { "linux-x86_64": Artifact };
  downloads: { "linux-bundle": Artifact };
}
export interface ManifestV3 extends ManifestFields {
  format: "maple-agent-update-v3";
  artifacts: { "windows-x86_64": Artifact };
  downloads: { windows: Artifact };
}
export interface ManifestV4 extends ManifestFields {
  format: "maple-agent-update-v4";
  artifacts: { "windows-aarch64": Artifact };
  downloads: { "windows-arm64": Artifact };
}
export type UpdateManifest = Manifest | ManifestV2 | ManifestV3 | ManifestV4;
export interface Envelope {
  manifest: string;
  signature: string;
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function keys(value: Record<string, unknown>, allowed: string[]): boolean {
  return Object.keys(value).every((key) => allowed.includes(key));
}
function fullMatch(pattern: RegExp, value: string): boolean {
  // JavaScript's `$` also matches before a final line terminator.
  return pattern.exec(value)?.[0] === value;
}
function boundedText(value: unknown, max: number): value is string {
  return typeof value === "string" && encoder.encode(value).length <= max;
}
function scalarText(value: string): boolean {
  // JSON may contain escaped lone UTF-16 surrogates despite valid UTF-8 bytes.
  for (const character of value) {
    const code = character.codePointAt(0)!;
    if (code >= 0xd800 && code <= 0xdfff) return false;
  }
  return true;
}
function artifact(
  value: unknown,
  version: string,
  suffix: string,
): value is Artifact {
  if (!record(value) || !keys(value, ["path", "size", "sha256"])) return false;
  const prefix = `/agent/stable/releases/${version}/`;
  if (typeof value.path !== "string" || !value.path.startsWith(prefix))
    return false;
  const file = value.path.slice(prefix.length);
  return (
    file.length <= 200 &&
    fullMatch(/^[A-Za-z0-9][A-Za-z0-9._-]*$/, file) &&
    file.endsWith(suffix) &&
    !file.includes("..") &&
    typeof value.size === "number" &&
    Number.isSafeInteger(value.size) &&
    value.size > 0 &&
    value.size <= MAX_ARTIFACT_BYTES &&
    typeof value.sha256 === "string" &&
    fullMatch(/^[0-9a-f]{64}$/, value.sha256)
  );
}

export function parseManifest(value: unknown): UpdateManifest {
  if (
    !record(value) ||
    !keys(value, [
      "format",
      "product",
      "channel",
      "version",
      "build",
      "source_sha",
      "pub_date",
      "notes",
      "artifacts",
      "downloads",
    ]) ||
    ![
      "maple-agent-update-v1",
      "maple-agent-update-v2",
      "maple-agent-update-v3",
      "maple-agent-update-v4",
    ].includes(value.format as string) ||
    value.product !== "maple-agent" ||
    value.channel !== "stable" ||
    typeof value.version !== "string" ||
    value.version.length > 64 ||
    !fullMatch(VERSION, value.version) ||
    typeof value.build !== "string" ||
    !fullMatch(/^[1-9][0-9]{0,17}$/, value.build) ||
    typeof value.source_sha !== "string" ||
    !fullMatch(/^[0-9a-f]{40}$/, value.source_sha) ||
    typeof value.pub_date !== "string" ||
    !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/.test(value.pub_date) ||
    !Number.isFinite(Date.parse(value.pub_date)) ||
    new Date(value.pub_date).toISOString().replace(".000Z", "Z") !==
      value.pub_date ||
    !boundedText(value.notes, 16 * 1024) ||
    !scalarText(value.notes) ||
    /[\x00-\x08\x0b-\x1f\x7f]/.test(value.notes) ||
    !record(value.artifacts) ||
    !record(value.downloads)
  )
    throw new Error("invalid manifest");
  if (value.format === "maple-agent-update-v2") {
    const linux = value.artifacts["linux-x86_64"];
    const download = value.downloads["linux-bundle"];
    if (
      !keys(value.artifacts, ["linux-x86_64"]) ||
      !keys(value.downloads, ["linux-bundle"]) ||
      !artifact(linux, value.version, ".tar.gz") ||
      !artifact(download, value.version, ".tar.gz") ||
      linux.path !== download.path ||
      linux.size !== download.size ||
      linux.sha256 !== download.sha256
    )
      throw new Error("invalid Linux bundle manifest");
    return value as unknown as ManifestV2;
  }
  if (value.format === "maple-agent-update-v3") {
    if (
      !keys(value.artifacts, ["windows-x86_64"]) ||
      !keys(value.downloads, ["windows"]) ||
      !artifact(value.artifacts["windows-x86_64"], value.version, ".tar.gz") ||
      !artifact(value.downloads.windows, value.version, ".exe")
    )
      throw new Error("invalid Windows bundle manifest");
    return value as unknown as ManifestV3;
  }
  if (value.format === "maple-agent-update-v4") {
    if (
      !keys(value.artifacts, ["windows-aarch64"]) ||
      !keys(value.downloads, ["windows-arm64"]) ||
      !artifact(value.artifacts["windows-aarch64"], value.version, ".tar.gz") ||
      !artifact(value.downloads["windows-arm64"], value.version, ".exe")
    )
      throw new Error("invalid Windows ARM64 bundle manifest");
    return value as unknown as ManifestV4;
  }
  // Keep the shipped v1 target/path rules, including optional AppImages.
  if (
    !keys(value.artifacts, ["darwin-aarch64", "linux-x86_64"]) ||
    !keys(value.downloads, ["macos", "linux-appimage"]) ||
    !artifact(
      value.artifacts["darwin-aarch64"],
      value.version,
      ".app.tar.gz",
    ) ||
    !artifact(value.downloads.macos, value.version, ".dmg")
  )
    throw new Error("invalid manifest");
  const linux = value.artifacts["linux-x86_64"];
  const download = value.downloads["linux-appimage"];
  if (
    (linux === undefined) !== (download === undefined) ||
    (linux !== undefined &&
      (!artifact(linux, value.version, ".AppImage") ||
        !artifact(download, value.version, ".AppImage") ||
        linux.path !== download.path ||
        linux.size !== download.size ||
        linux.sha256 !== download.sha256))
  ) {
    throw new Error("invalid Linux artifact");
  }
  return value as unknown as Manifest;
}

// The Worker validates public metadata structure. Clients and the publisher
// authenticate exact bytes with pinned keys; R2 write authority is not signing authority.
export function parseEnvelope(bytes: Uint8Array): {
  envelope: Envelope;
  manifest: UpdateManifest;
} {
  if (bytes.length === 0 || bytes.length > MAX_ENVELOPE_BYTES)
    throw new Error("oversized envelope");
  const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  const envelope: unknown = JSON.parse(text);
  if (
    !record(envelope) ||
    !keys(envelope, ["manifest", "signature"]) ||
    !boundedText(envelope.manifest, MAX_MANIFEST_BYTES) ||
    !boundedText(envelope.signature, MAX_SIGNATURE_BYTES)
  )
    throw new Error("invalid envelope");
  const manifest = parseManifest(JSON.parse(envelope.manifest));
  const lines = envelope.signature.trimEnd().split("\n");
  if (
    lines.length !== 4 ||
    !lines[0].startsWith("untrusted comment: ") ||
    lines[2] !==
      `trusted comment: product=maple-agent channel=stable version=${manifest.version}` ||
    !fullMatch(/^[A-Za-z0-9+/]+={0,2}$/, lines[1]) ||
    !fullMatch(/^[A-Za-z0-9+/]+={0,2}$/, lines[3])
  )
    throw new Error("invalid signature format");
  const signature = atob(lines[1]);
  if (
    signature.length !== 74 ||
    signature.slice(0, 2) !== "ED" ||
    atob(lines[3]).length !== 64
  )
    throw new Error("unsupported signature format");
  return { envelope: envelope as unknown as Envelope, manifest };
}

export function releasePath(
  path: string,
): { version: string; file: string; key: string } | null {
  const match =
    /^\/agent\/stable\/releases\/([^/]+)\/([A-Za-z0-9][A-Za-z0-9._-]{0,199})$/.exec(
      path,
    );
  if (
    !match ||
    match[1].length > 64 ||
    !fullMatch(VERSION, match[1]) ||
    match[0] !== path ||
    match[2].includes("..")
  )
    return null;
  return {
    version: match[1],
    file: match[2],
    key: `stable/releases/${match[1]}/${match[2]}`,
  };
}
