/** Public compaction metadata only. Summary text and opaque checkpoint content never enter Chat. */
export type ChatCompactionMarker = {
  id: string;
  type: "compaction";
  status: "completed";
  after_item_id: string | null;
  response_id: string;
  created_at: number;
  generation: number;
};

export type ChatCompactionPage = {
  data: ChatCompactionMarker[];
  has_more: boolean;
  first_id: string | null;
  last_id: string | null;
};

type TimelineItem = {
  id: string;
  type: string;
  response_id?: string;
  role?: string;
};

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const MAX_COMPACTION_PAGE_ITEMS = 100;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isUuid(value: unknown): value is string {
  return typeof value === "string" && UUID.test(value);
}

function invalidMetadata(): never {
  // Do not include the rejected payload: it could contain private checkpoint data.
  throw new Error("Invalid conversation compaction metadata");
}

export function parseCompactionPage(value: unknown): ChatCompactionPage {
  if (
    !isRecord(value) ||
    value.object !== "list" ||
    !Array.isArray(value.data) ||
    value.data.length > MAX_COMPACTION_PAGE_ITEMS ||
    typeof value.has_more !== "boolean"
  ) {
    return invalidMetadata();
  }

  const data = value.data.map((item): ChatCompactionMarker => {
    if (
      !isRecord(item) ||
      item.object !== "conversation.compaction" ||
      item.status !== "completed" ||
      !isUuid(item.id) ||
      !isUuid(item.response_id) ||
      (item.after_item_id !== null && !isUuid(item.after_item_id)) ||
      typeof item.created_at !== "number" ||
      !Number.isSafeInteger(item.created_at) ||
      item.created_at < 0 ||
      typeof item.generation !== "number" ||
      !Number.isSafeInteger(item.generation) ||
      item.generation <= 0
    ) {
      return invalidMetadata();
    }
    return {
      id: item.id,
      type: "compaction",
      status: "completed",
      after_item_id: item.after_item_id,
      response_id: item.response_id,
      created_at: item.created_at,
      generation: item.generation
    };
  });

  const firstId = data[0]?.id ?? null;
  const lastId = data.at(-1)?.id ?? null;
  if (
    value.first_id !== firstId ||
    value.last_id !== lastId ||
    (value.has_more && !lastId) ||
    new Set(data.map((item) => item.id)).size !== data.length
  ) {
    return invalidMetadata();
  }

  return { data, has_more: value.has_more, first_id: firstId, last_id: lastId };
}

/**
 * Standard Responses progress is advisory. Its item may not become a durable
 * checkpoint (for example when optional compaction falls back to a fitting
 * original prompt). Copy no summary/ciphertext, and never add it to history.
 * Undefined leaves the current indicator unchanged, including on old servers
 * and unknown future events.
 */
export function chatCompactionProgressUpdate(event: unknown): boolean | undefined {
  if (!isRecord(event) || typeof event.type !== "string") return undefined;
  if (event.type === "response.compaction.compacting") {
    return isUuid(event.item_id) &&
      Number.isSafeInteger(event.sequence_number) &&
      (event.sequence_number as number) >= 0 &&
      Number.isSafeInteger(event.output_index) &&
      (event.output_index as number) >= 0
      ? true
      : undefined;
  }

  switch (event.type) {
    case "response.output_text.delta":
    case "response.reasoning_text.delta":
      return typeof event.delta === "string" && event.delta.length > 0 ? false : undefined;
    case "response.created":
    case "response.output_item.added":
    case "response.output_item.done":
    case "response.content_part.added":
    case "response.content_part.done":
    case "response.output_text.done":
    case "response.reasoning_text.done":
    case "response.web_search_call.in_progress":
    case "response.web_search_call.searching":
    case "response.web_search_call.completed":
    case "tool_call.created":
    case "tool_output.created":
    case "response.completed":
    case "response.failed":
    case "response.incomplete":
    case "response.error":
    case "response.cancelled":
    case "error":
      return false;
    default:
      return undefined;
  }
}

export function mergeChatCompactionMarkers(
  current: readonly ChatCompactionMarker[],
  incoming: readonly ChatCompactionMarker[]
): ChatCompactionMarker[] {
  const markers = new Map(current.map((marker) => [marker.id, marker]));
  for (const marker of incoming) markers.set(marker.id, marker);
  return [...markers.values()].sort((left, right) => left.generation - right.generation);
}

/**
 * Add dividers to the loaded view without changing canonical history or its cursors.
 * An absent tool-step anchor stays absent until that page of history is loaded.
 */
export function projectChatCompactions<T extends TimelineItem>(
  items: readonly T[],
  markers: readonly ChatCompactionMarker[]
): Array<T | ChatCompactionMarker> {
  const canonical = items.filter((item) => item.type !== "compaction");
  const canonicalIds = new Set(canonical.map((item) => item.id));
  const after = new Map<string, ChatCompactionMarker[]>();
  const before = new Map<string, ChatCompactionMarker[]>();
  const seen = new Set<string>();
  const firstItemByResponse = new Map<string, T>();
  const userByResponse = new Map<string, T>();
  for (const item of canonical) {
    if (!item.response_id) continue;
    if (!firstItemByResponse.has(item.response_id)) firstItemByResponse.set(item.response_id, item);
    if (item.type === "message" && item.role === "user" && !userByResponse.has(item.response_id)) {
      userByResponse.set(item.response_id, item);
    }
  }

  const add = (
    destination: Map<string, ChatCompactionMarker[]>,
    anchor: string,
    marker: ChatCompactionMarker
  ) => {
    const bucket = destination.get(anchor) ?? [];
    bucket.push(marker);
    destination.set(anchor, bucket);
  };

  for (const marker of [...markers].sort((left, right) => left.generation - right.generation)) {
    if (seen.has(marker.id) || canonicalIds.has(marker.id)) continue;
    seen.add(marker.id);
    if (marker.after_item_id !== null) {
      if (canonicalIds.has(marker.after_item_id)) add(after, marker.after_item_id, marker);
      continue;
    }

    // Older checkpoints may have no exact item anchor. Their user turn is
    // the natural place for the divider once that page is available.
    const user = userByResponse.get(marker.response_id);
    if (user) {
      add(after, user.id, marker);
      continue;
    }
    const firstOutput = firstItemByResponse.get(marker.response_id);
    if (firstOutput) add(before, firstOutput.id, marker);
  }

  return canonical.flatMap(
    (item): Array<T | ChatCompactionMarker> => [
      ...(before.get(item.id) ?? []),
      item,
      ...(after.get(item.id) ?? [])
    ]
  );
}
