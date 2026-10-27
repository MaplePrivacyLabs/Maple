import type { ManifestV3 } from "./manifest";
import { fixture, PAYLOAD } from "./fixtures";

export function encodeWindowsFixture(
  manifest: ManifestV3,
): Uint8Array<ArrayBuffer> {
  const envelope = JSON.parse(new TextDecoder().decode(fixture().bytes));
  envelope.manifest = JSON.stringify(manifest);
  envelope.signature = envelope.signature.replace(
    "version=0.0.2",
    `version=${manifest.version}`,
  );
  return new TextEncoder().encode(JSON.stringify(envelope));
}

export function windowsFixture(): {
  manifest: ManifestV3;
  bytes: Uint8Array<ArrayBuffer>;
} {
  const artifact = (name: string) => ({
    path: `/agent/stable/releases/0.0.2/${name}`,
    size: PAYLOAD.length,
    sha256: "d".repeat(64),
  });
  const manifest: ManifestV3 = {
    ...fixture().manifest,
    format: "maple-agent-update-v3",
    artifacts: {
      "windows-x86_64": artifact("Maple-Agent-windows-x86_64.tar.gz"),
    },
    downloads: { windows: artifact("Maple-Agent-windows-x86_64-setup.exe") },
  };
  return { manifest, bytes: encodeWindowsFixture(manifest) };
}
