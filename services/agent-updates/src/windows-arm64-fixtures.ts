import type { ManifestV4 } from "./manifest";
import { fixture, PAYLOAD } from "./fixtures";

export function encodeWindowsArm64Fixture(
  manifest: ManifestV4,
): Uint8Array<ArrayBuffer> {
  const envelope = JSON.parse(new TextDecoder().decode(fixture().bytes));
  envelope.manifest = JSON.stringify(manifest);
  envelope.signature = envelope.signature.replace(
    "version=0.0.2",
    `version=${manifest.version}`,
  );
  return new TextEncoder().encode(JSON.stringify(envelope));
}

export function windowsArm64Fixture(): {
  manifest: ManifestV4;
  bytes: Uint8Array<ArrayBuffer>;
} {
  const artifact = (name: string) => ({
    path: `/agent/stable/releases/0.0.2/${name}`,
    size: PAYLOAD.length,
    sha256: "e".repeat(64),
  });
  const manifest: ManifestV4 = {
    ...fixture().manifest,
    format: "maple-agent-update-v4",
    artifacts: {
      "windows-aarch64": artifact("Maple-Agent-windows-aarch64.tar.gz"),
    },
    downloads: {
      "windows-arm64": artifact("Maple-Agent-windows-aarch64-setup.exe"),
    },
  };
  return { manifest, bytes: encodeWindowsArm64Fixture(manifest) };
}
