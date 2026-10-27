import {
  trackResponse,
  consumeEvents,
  type TrackingEnv,
  type TrackingContext,
  type EventDatabase,
  type EventBatch,
} from "../../download-tracking/tracking";
import {
  MAX_ENVELOPE_BYTES,
  parseEnvelope,
  releasePath,
  type Artifact,
  type UpdateManifest,
} from "./manifest";

interface StoredObject {
  size: number;
  httpEtag: string;
  body?: ReadableStream<Uint8Array>;
}
export interface Bucket {
  get(
    key: string,
    options?: { range: { offset: number; length: number } },
  ): Promise<StoredObject | null>;
  head(key: string): Promise<StoredObject | null>;
}
export interface Env extends TrackingEnv {
  AGENT_RELEASES: Bucket;
  DOWNLOAD_DB?: EventDatabase;
}

const NO_STORE = {
  "cache-control": "no-store",
  "cdn-cache-control": "no-store",
  "x-content-type-options": "nosniff",
};
function response(
  status: number,
  method: string,
  extra?: HeadersInit,
): Response {
  return new Response(method === "HEAD" ? null : `${status}\n`, {
    status,
    headers: {
      ...NO_STORE,
      "content-type": "text/plain; charset=utf-8",
      ...extra,
    },
  });
}
async function metadata<Format extends UpdateManifest["format"]>(
  bucket: Bucket,
  key: string,
  format: Format,
): Promise<{
  bytes: Uint8Array<ArrayBuffer>;
  manifest: Extract<UpdateManifest, { format: Format }>;
} | null> {
  const object = await bucket.get(key);
  if (!object) return null;
  if (object.size === 0 || object.size > MAX_ENVELOPE_BYTES || !object.body) {
    await object.body?.cancel().catch(() => {});
    throw new Error("metadata size");
  }
  const reader = object.body.getReader();
  let length = 0;
  const chunks: Uint8Array[] = [];
  try {
    while (true) {
      const part = await reader.read();
      if (part.done) break;
      length += part.value.length;
      if (length > MAX_ENVELOPE_BYTES || length > object.size)
        throw new Error("metadata size");
      chunks.push(part.value);
    }
  } catch (error) {
    await reader.cancel().catch(() => {});
    throw error;
  }
  if (length !== object.size) throw new Error("truncated metadata");
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.length;
  }
  const manifest = parseEnvelope(bytes).manifest;
  if (manifest.format !== format) throw new Error("manifest format mismatch");
  return {
    bytes,
    manifest: manifest as Extract<UpdateManifest, { format: Format }>,
  };
}

function metadataResponse(
  request: Request,
  bytes: Uint8Array<ArrayBuffer>,
  immutable = false,
): Response {
  return new Response(request.method === "HEAD" ? null : bytes, {
    headers: {
      ...(immutable
        ? { "cache-control": "public, max-age=31536000, immutable" }
        : NO_STORE),
      "content-type": "application/json; charset=utf-8",
      "content-length": String(bytes.length),
      "x-content-type-options": "nosniff",
    },
  });
}

function requestedRange(
  header: string,
  size: number,
): { offset: number; length: number } | null {
  const match = /^bytes=(\d*)-(\d*)$/.exec(header);
  if (!match || (!match[1] && !match[2])) return null;
  if (!match[1]) {
    const suffix = Number(match[2]);
    return Number.isSafeInteger(suffix) && suffix > 0
      ? { offset: Math.max(0, size - suffix), length: Math.min(size, suffix) }
      : null;
  }
  const start = Number(match[1]);
  const end = match[2] ? Number(match[2]) : size - 1;
  if (
    !Number.isSafeInteger(start) ||
    !Number.isSafeInteger(end) ||
    start >= size ||
    end < start
  )
    return null;
  return { offset: start, length: Math.min(end, size - 1) - start + 1 };
}

function contentType(path: string): string {
  if (path.endsWith(".dmg")) return "application/x-apple-diskimage";
  if (path.endsWith(".tar.gz")) return "application/gzip";
  return "application/octet-stream";
}
async function artifactResponse(
  request: Request,
  bucket: Bucket,
  key: string,
  artifact: Artifact,
): Promise<Response> {
  const object = await bucket.head(key);
  if (!object) return response(404, request.method);
  if (object.size !== artifact.size) throw new Error("artifact size mismatch");
  const headers = new Headers({
    "cache-control": "public, max-age=31536000, immutable",
    "content-type": contentType(artifact.path),
    "x-content-type-options": "nosniff",
    "accept-ranges": "bytes",
    etag: object.httpEtag,
    "content-length": String(object.size),
  });
  if (
    request.headers
      .get("if-none-match")
      ?.split(",")
      .some((value) => value.trim() === object.httpEtag || value.trim() === "*")
  )
    return new Response(null, { status: 304, headers });
  // RFC 9110: Range modifies GET only; HEAD describes the full representation.
  const rangeHeader =
    request.method === "GET" ? request.headers.get("range") : null;
  const ifRange = request.headers.get("if-range");
  const range =
    rangeHeader && (!ifRange || ifRange === object.httpEtag)
      ? requestedRange(rangeHeader, object.size)
      : undefined;
  if (range === null)
    return response(416, request.method, {
      "content-range": `bytes */${object.size}`,
      "accept-ranges": "bytes",
    });
  if (range) {
    headers.set(
      "content-range",
      `bytes ${range.offset}-${range.offset + range.length - 1}/${object.size}`,
    );
    headers.set("content-length", String(range.length));
  }
  const status = range ? 206 : 200;
  if (request.method === "HEAD") return new Response(null, { status, headers });
  const full = await bucket.get(key, range ? { range } : undefined);
  if (
    !full ||
    !full.body ||
    full.size !== object.size ||
    full.httpEtag !== object.httpEtag
  ) {
    await full?.body?.cancel().catch(() => {});
    throw new Error("artifact changed");
  }
  return new Response(full.body, { status, headers });
}

const deliveryWorker = {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname;
    // No origin fetch: missing data and exceptions remain inside this Worker.
    if (
      !path.startsWith("/agent/stable/") &&
      !path.startsWith("/download/agent/stable/")
    )
      return response(404, request.method);
    if (!["GET", "HEAD"].includes(request.method))
      return response(405, request.method, { allow: "GET, HEAD" });
    if (
      url.search ||
      path.includes("%") ||
      path.includes("\\") ||
      path.includes("//")
    )
      return response(404, request.method);
    try {
      if (
        path === "/agent/stable/windows/latest.json" ||
        path === "/download/agent/stable/windows"
      ) {
        const current = await metadata(
          env.AGENT_RELEASES,
          "stable/windows/latest.json",
          "maple-agent-update-v3",
        );
        if (!current) return response(404, request.method);
        return path.startsWith("/download/")
          ? new Response(null, {
              status: 302,
              headers: {
                ...NO_STORE,
                location: current.manifest.downloads.windows.path,
              },
            })
          : metadataResponse(request, current.bytes);
      }
      if (
        path === "/agent/stable/linux/latest.json" ||
        path === "/download/agent/stable/linux"
      ) {
        const current = await metadata(
          env.AGENT_RELEASES,
          "stable/linux/latest.json",
          "maple-agent-update-v2",
        );
        if (!current) return response(404, request.method);
        return path.startsWith("/download/")
          ? new Response(null, {
              status: 302,
              headers: {
                ...NO_STORE,
                location: current.manifest.downloads["linux-bundle"].path,
              },
            })
          : metadataResponse(request, current.bytes);
      }
      if (
        path === "/agent/stable/latest.json" ||
        path.startsWith("/download/agent/stable/")
      ) {
        if (
          path.startsWith("/download/") &&
          ![
            "/download/agent/stable/macos",
            "/download/agent/stable/linux-appimage",
          ].includes(path)
        )
          return response(404, request.method);
        const current = await metadata(
          env.AGENT_RELEASES,
          "stable/latest.json",
          "maple-agent-update-v1",
        );
        if (!current) return response(404, request.method);
        if (path.startsWith("/download/")) {
          const target = path.endsWith("/macos")
            ? current.manifest.downloads.macos
            : current.manifest.downloads["linux-appimage"];
          return target
            ? new Response(null, {
                status: 302,
                headers: { ...NO_STORE, location: target.path },
              })
            : response(404, request.method);
        }
        return metadataResponse(request, current.bytes);
      }
      const release = releasePath(path);
      if (!release) return response(404, request.method);
      // Explicit metadata routes never fall back to their sibling feed. Other
      // paths are authorized only by exact enumeration in these fixed keys.
      const candidates =
        release.file === "manifest.json"
          ? (["maple-agent-update-v1"] as const)
          : release.file === "linux-manifest.json"
            ? (["maple-agent-update-v2"] as const)
            : release.file === "windows-manifest.json"
              ? (["maple-agent-update-v3"] as const)
              : ([
                  "maple-agent-update-v1",
                  "maple-agent-update-v2",
                  "maple-agent-update-v3",
                ] as const);
      for (const format of candidates) {
        const file =
          format === "maple-agent-update-v1"
            ? "manifest.json"
            : format === "maple-agent-update-v2"
              ? "linux-manifest.json"
              : "windows-manifest.json";
        const published = await metadata(
          env.AGENT_RELEASES,
          `stable/releases/${release.version}/${file}`,
          format,
        );
        if (!published) continue;
        if (published.manifest.version !== release.version)
          throw new Error("version mismatch");
        if (release.file === file)
          return metadataResponse(request, published.bytes, true);
        const artifact = [
          ...Object.values(published.manifest.artifacts),
          ...Object.values(published.manifest.downloads),
        ].find((value) => value.path === path);
        // A hit retains shipped behavior without consulting later platform
        // manifests. Missing/nonmatching earlier formats permit the next;
        // corrupt consulted metadata never falls through.
        if (artifact)
          return await artifactResponse(
            request,
            env.AGENT_RELEASES,
            release.key,
            artifact,
          );
      }
      return response(404, request.method);
    } catch {
      return response(503, request.method);
    }
  },
};

export default {
  async fetch(
    request: Request,
    env: Env,
    ctx?: TrackingContext,
  ): Promise<Response> {
    const result = await deliveryWorker.fetch(request, env);
    if (ctx) trackResponse(request, result, env, ctx, "agent");
    return result;
  },
  async queue(batch: EventBatch, env: Env): Promise<void> {
    if (!env.DOWNLOAD_DB)
      throw new Error("download tracking database binding missing");
    await consumeEvents(batch, { DOWNLOAD_DB: env.DOWNLOAD_DB });
  },
};
