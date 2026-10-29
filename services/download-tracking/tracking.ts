import repositoryMetadata from "../../repo.meta.json";

export type Product = "agent" | "research";
type Platform = "macos" | "windows" | "linux" | "android" | "all";
type Artifact =
  | "macos-dmg"
  | "macos-updater"
  | "windows-installer"
  | "windows-bundle"
  | "linux-bundle"
  | "linux-appimage"
  | "linux-deb"
  | "linux-rpm"
  | "android-apk"
  | "latest"
  | "manifest";
type Kind = "artifact" | "alias" | "metadata";
type Range = "none" | "requested" | "partial" | "unsatisfiable";
type Result =
  "success" | "redirect" | "not_modified" | "client_error" | "server_error";
type ClientKind = "browser" | "maple" | "cli" | "other";
const HOSTS = ["updates.maple.ai", "updates.trymaple.ai"] as const;
const VERSION = /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/;
const MAX_RESPONSE_LENGTH = 4 * 1024 ** 3;
const RESEARCH_RELEASE = `/${repositoryMetadata.github.owner}/${repositoryMetadata.github.repository}/releases/download/`;

/** A response observation, not a user identity or proof of transfer/install. */
export interface DownloadEvent {
  schema_version: 1;
  event_id: string;
  observed_at: string;
  product: Product;
  host: (typeof HOSTS)[number];
  version: string | null;
  platform: Platform;
  artifact: Artifact;
  kind: Kind;
  method: "GET" | "HEAD";
  status: number;
  range: Range;
  result: Result;
  response_length: number | null;
  client_kind: ClientKind;
  is_probe: 0 | 1;
}

// Structural interfaces keep this module shared by both existing Workers.
export interface EventQueue {
  send(event: DownloadEvent): Promise<void>;
}
export interface TrackingEnv {
  DOWNLOAD_EVENTS?: EventQueue;
}
export interface TrackingContext {
  waitUntil(promise: Promise<unknown>): void;
}
export interface EventMessage {
  body: unknown;
  ack(): void;
  retry(): void;
}
export interface EventBatch {
  messages: readonly EventMessage[];
}
export interface EventDatabase {
  prepare(sql: string): {
    bind(...values: (string | number | null)[]): {
      run(): Promise<{ success: boolean }>;
    };
  };
}

interface Route {
  kind: Kind;
  version: string | null;
  platform: Platform;
  artifact: Artifact;
}

function stableVersion(value: string): boolean {
  return value.length <= 64 && VERSION.exec(value)?.[0] === value;
}

function agentArtifact(path: string): Route | null {
  const match =
    /^\/agent\/stable\/releases\/([^/]+)\/([A-Za-z0-9][A-Za-z0-9._-]*)$/.exec(
      path,
    );
  if (!match || !stableVersion(match[1])) return null;
  const file = match[2];
  if (file.length > 200 || file.includes("..")) return null;
  const base = { version: match[1], kind: "artifact" as const };
  if (file.endsWith("-windows-x86_64.tar.gz"))
    return { ...base, platform: "windows", artifact: "windows-bundle" };
  if (file.endsWith("-windows-x86_64-setup.exe"))
    return { ...base, platform: "windows", artifact: "windows-installer" };
  // Wrong Windows profiles must not fall through to Linux's historical generic
  // tar suffix. Classification does not authorize serving any artifact.
  if (file.toLowerCase().includes("windows")) return null;
  if (file.endsWith(".app.tar.gz"))
    return { ...base, platform: "macos", artifact: "macos-updater" };
  if (file.endsWith(".dmg"))
    return { ...base, platform: "macos", artifact: "macos-dmg" };
  if (file.endsWith(".AppImage"))
    return { ...base, platform: "linux", artifact: "linux-appimage" };
  if (file.endsWith(".tar.gz"))
    return { ...base, platform: "linux", artifact: "linux-bundle" };
  return null;
}

const AGENT_ALIASES: Record<string, Pick<Route, "platform" | "artifact">> = {
  macos: { platform: "macos", artifact: "macos-dmg" },
  linux: { platform: "linux", artifact: "linux-bundle" },
  windows: { platform: "windows", artifact: "windows-installer" },
  "linux-appimage": { platform: "linux", artifact: "linux-appimage" },
};
const RESEARCH_ALIASES: Record<string, Pick<Route, "platform" | "artifact">> = {
  macos: { platform: "macos", artifact: "macos-dmg" },
  windows: { platform: "windows", artifact: "windows-installer" },
  "linux-appimage": { platform: "linux", artifact: "linux-appimage" },
  "linux-deb": { platform: "linux", artifact: "linux-deb" },
  "linux-rpm": { platform: "linux", artifact: "linux-rpm" },
  android: { platform: "android", artifact: "android-apk" },
};

function researchVersion(location: URL, artifact: Artifact): string | null {
  if (
    location.origin !== "https://github.com" ||
    !location.pathname.startsWith(RESEARCH_RELEASE)
  )
    return null;
  const match = /^v([^/]+)\/([^/]+)$/.exec(
    location.pathname.slice(RESEARCH_RELEASE.length),
  );
  if (!match || !stableVersion(match[1])) return null;
  const [version, file] = match.slice(1);
  const names: Partial<Record<Artifact, string>> = {
    "macos-dmg": `Maple_${version}_universal.dmg`,
    "windows-installer": `Maple_${version}_x64-setup.exe`,
    "linux-appimage": `Maple_${version}_amd64.AppImage`,
    "linux-deb": `Maple_${version}_amd64.deb`,
    "android-apk": "app-universal-release.apk",
  };
  const matches =
    artifact === "linux-rpm"
      ? new RegExp(
          `^Maple-${version.replaceAll(".", "\\.")}-[1-9][0-9]*\\.x86_64\\.rpm$`,
        ).exec(file)?.[0] === file
      : names[artifact] === file;
  return matches ? version : null;
}

function routeFor(
  url: URL,
  response: Response,
  product: Product,
): Route | null {
  const path = url.pathname;
  // Research accepts marketing queries; classify only its pathname and never
  // retain query values. Agent delivery itself rejects queries.
  if (
    (product === "agent" && url.search) ||
    url.hash ||
    path.includes("%") ||
    path.includes("\\") ||
    path.includes("//")
  )
    return null;
  if (product === "research" && path === "/latest.json")
    return {
      kind: "metadata",
      version: null,
      platform: "all",
      artifact: "latest",
    };
  if (product === "agent") {
    if (
      path === "/agent/stable/latest.json" ||
      path === "/agent/stable/linux/latest.json" ||
      path === "/agent/stable/windows/latest.json"
    )
      return {
        kind: "metadata",
        version: null,
        platform: path.includes("/linux/")
          ? "linux"
          : path.includes("/windows/")
            ? "windows"
            : "macos",
        artifact: "latest",
      };
    const manifest =
      /^\/agent\/stable\/releases\/([^/]+)\/(linux-manifest|windows-manifest|manifest)\.json$/.exec(
        path,
      );
    if (manifest && stableVersion(manifest[1]))
      return {
        kind: "metadata",
        version: manifest[1],
        platform:
          manifest[2] === "linux-manifest"
            ? "linux"
            : manifest[2] === "windows-manifest"
              ? "windows"
              : "macos",
        artifact: "manifest",
      };
    const artifact = agentArtifact(path);
    if (artifact) return artifact;
  }
  const prefix = `/download/${product}/stable/`;
  if (!path.startsWith(prefix)) return null;
  const aliases = product === "agent" ? AGENT_ALIASES : RESEARCH_ALIASES;
  const alias = aliases[path.slice(prefix.length)];
  if (!Object.hasOwn(aliases, path.slice(prefix.length)) || !alias) return null;
  let version: string | null = null;
  const target = response.headers.get("location");
  if (response.status === 302 && target && target.length <= 512) {
    const location = new URL(target, url);
    if (
      !location.search &&
      !location.hash &&
      !location.username &&
      !location.password
    ) {
      if (product === "agent" && location.origin === url.origin) {
        const artifact = agentArtifact(location.pathname);
        if (artifact?.artifact === alias.artifact) version = artifact.version;
      } else if (product === "research") {
        version = researchVersion(location, alias.artifact);
      }
    }
  }
  return { kind: "alias", version, ...alias };
}

function clientKind(request: Request): ClientKind {
  const ua = request.headers.get("user-agent")?.slice(0, 512) ?? "";
  if (/maple.*updat/i.test(ua)) return "maple";
  if (/Mozilla\/5\.0/i.test(ua)) return "browser";
  if (/curl|wget|python-urllib|Go-http-client/i.test(ua)) return "cli";
  return "other";
}

function responseResult(status: number): Result {
  if (status === 304) return "not_modified";
  if (status >= 500) return "server_error";
  if (status >= 400) return "client_error";
  if (status >= 300) return "redirect";
  return "success";
}

function logFailure(code: string): void {
  // Deliberately never log the exception, request, URL, headers or event body.
  console.error(JSON.stringify({ service: "download-tracking", code }));
}

async function enqueue(queue: EventQueue, event: DownloadEvent): Promise<void> {
  // An ambiguous send can have succeeded: keep the ID for D1 deduplication.
  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      await queue.send(event);
      return;
    } catch {
      logFailure("queue_send_failed");
    }
  }
}

/** Best-effort, unsampled response-start capture. Never waits for/reads a body. */
export function trackResponse(
  request: Request,
  response: Response,
  env: TrackingEnv,
  ctx: TrackingContext,
  product: Product,
): void {
  try {
    if (!["GET", "HEAD"].includes(request.method)) return;
    const url = new URL(request.url);
    if (
      url.protocol !== "https:" ||
      !HOSTS.includes(url.host as (typeof HOSTS)[number]) ||
      url.username ||
      url.password ||
      response.status < 200 ||
      response.status > 599
    )
      return;
    const route = routeFor(url, response, product);
    if (!route) return;
    const queue = env.DOWNLOAD_EVENTS;
    if (!queue) {
      logFailure("queue_binding_missing");
      return;
    }
    const length = response.headers.get("content-length");
    const parsedLength =
      length && /^(0|[1-9][0-9]*)$/.test(length) ? Number(length) : null;
    const event: DownloadEvent = {
      schema_version: 1,
      event_id: crypto.randomUUID(),
      observed_at: new Date().toISOString(),
      product,
      host: url.host as DownloadEvent["host"],
      ...route,
      method: request.method as "GET" | "HEAD",
      status: response.status,
      range:
        route.kind !== "artifact"
          ? "none"
          : response.status === 206
            ? "partial"
            : response.status === 416
              ? "unsatisfiable"
              : request.headers.has("range")
                ? "requested"
                : "none",
      result: responseResult(response.status),
      response_length:
        parsedLength !== null &&
        Number.isSafeInteger(parsedLength) &&
        parsedLength <= MAX_RESPONSE_LENGTH
          ? parsedLength
          : null,
      client_kind: clientKind(request),
      is_probe: request.headers.get("x-maple-download-probe") === "1" ? 1 : 0,
    };
    // Deferring send also catches synchronous binding failures without affecting
    // delivery. waitUntil is bounded by the runtime; fail-open can leave gaps.
    ctx.waitUntil(Promise.resolve().then(() => enqueue(queue, event)));
  } catch {
    logFailure("capture_failed");
  }
}

const EVENT_KEYS = [
  "schema_version",
  "event_id",
  "observed_at",
  "product",
  "host",
  "version",
  "platform",
  "artifact",
  "kind",
  "method",
  "status",
  "range",
  "result",
  "response_length",
  "client_kind",
  "is_probe",
];

/** Queue content is a separate trust boundary, even with a private producer. */
export function isDownloadEvent(value: unknown): value is DownloadEvent {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const event = value as Record<string, unknown>;
  if (
    Object.keys(event).length !== EVENT_KEYS.length ||
    !EVENT_KEYS.every((key) => Object.hasOwn(event, key)) ||
    event.schema_version !== 1 ||
    typeof event.event_id !== "string" ||
    !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
      event.event_id,
    ) ||
    typeof event.observed_at !== "string" ||
    !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(event.observed_at) ||
    !Number.isFinite(Date.parse(event.observed_at)) ||
    new Date(event.observed_at).toISOString() !== event.observed_at ||
    !["agent", "research"].includes(event.product as string) ||
    !HOSTS.includes(event.host as (typeof HOSTS)[number]) ||
    !(
      event.version === null ||
      (typeof event.version === "string" && stableVersion(event.version))
    ) ||
    !["macos", "windows", "linux", "android", "all"].includes(
      event.platform as string,
    ) ||
    ![
      "macos-dmg",
      "macos-updater",
      "windows-installer",
      "windows-bundle",
      "linux-bundle",
      "linux-appimage",
      "linux-deb",
      "linux-rpm",
      "android-apk",
      "latest",
      "manifest",
    ].includes(event.artifact as string) ||
    !["artifact", "alias", "metadata"].includes(event.kind as string) ||
    !["GET", "HEAD"].includes(event.method as string) ||
    typeof event.status !== "number" ||
    !Number.isInteger(event.status) ||
    event.status < 200 ||
    event.status > 599 ||
    !["none", "requested", "partial", "unsatisfiable"].includes(
      event.range as string,
    ) ||
    event.result !== responseResult(event.status) ||
    !(
      event.response_length === null ||
      (typeof event.response_length === "number" &&
        Number.isSafeInteger(event.response_length) &&
        event.response_length >= 0 &&
        event.response_length <= MAX_RESPONSE_LENGTH)
    ) ||
    !["browser", "maple", "cli", "other"].includes(
      event.client_kind as string,
    ) ||
    ![0, 1].includes(event.is_probe as number)
  )
    return false;
  // The ledger accepts only combinations produced by our route classifier.
  const artifactPlatform: Record<Artifact, Platform[]> = {
    "macos-dmg": ["macos"],
    "macos-updater": ["macos"],
    "windows-installer": ["windows"],
    "windows-bundle": ["windows"],
    "linux-bundle": ["linux"],
    "linux-appimage": ["linux"],
    "linux-deb": ["linux"],
    "linux-rpm": ["linux"],
    "android-apk": ["android"],
    latest: ["all", "macos", "linux", "windows"],
    manifest: ["macos", "linux", "windows"],
  };
  if (
    !artifactPlatform[event.artifact as Artifact].includes(
      event.platform as Platform,
    )
  )
    return false;
  if (event.kind === "metadata") {
    if (
      !["latest", "manifest"].includes(event.artifact as string) ||
      event.range !== "none"
    )
      return false;
    if (event.artifact === "latest" && event.version !== null) return false;
    if (event.artifact === "manifest" && event.version === null) return false;
    return event.product === "research"
      ? event.artifact === "latest" && event.platform === "all"
      : event.platform !== "all";
  }
  if (["latest", "manifest"].includes(event.artifact as string)) return false;
  if (event.kind === "alias" && event.range !== "none") return false;
  if (event.product === "research")
    return (
      event.kind === "alias" &&
      !["macos-updater", "linux-bundle", "windows-bundle"].includes(
        event.artifact as string,
      )
    );
  if (
    ["linux-deb", "linux-rpm", "android-apk"].includes(event.artifact as string)
  )
    return false;
  return event.kind === "artifact"
    ? event.version !== null
    : !["macos-updater", "windows-bundle"].includes(event.artifact as string);
}

const INSERT = `INSERT INTO download_events
  (event_id, observed_at, product, host, version, platform, artifact, kind,
   method, status, range, result, response_length, client_kind, is_probe)
  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
  ON CONFLICT(event_id) DO NOTHING`;

/** Retries/DLQ are configured on the Queue; never acknowledge lost writes. */
export async function consumeEvents(
  batch: EventBatch,
  env: { DOWNLOAD_DB: EventDatabase },
): Promise<void> {
  for (const message of batch.messages) {
    if (!isDownloadEvent(message.body)) {
      logFailure("invalid_event");
      message.retry();
      continue;
    }
    const event = message.body;
    try {
      const result = await env.DOWNLOAD_DB.prepare(INSERT)
        .bind(
          event.event_id,
          event.observed_at,
          event.product,
          event.host,
          event.version,
          event.platform,
          event.artifact,
          event.kind,
          event.method,
          event.status,
          event.range,
          event.result,
          event.response_length,
          event.client_kind,
          event.is_probe,
        )
        .run();
      if (!result.success) throw new Error("write failed");
      message.ack();
    } catch {
      logFailure("database_write_failed");
      message.retry();
    }
  }
}
