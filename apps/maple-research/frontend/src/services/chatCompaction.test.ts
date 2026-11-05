import { describe, expect, test } from "bun:test";
import {
  chatCompactionProgressUpdate,
  mergeChatCompactionMarkers,
  parseCompactionPage,
  projectChatCompactions,
  type ChatCompactionMarker
} from "./chatCompaction";

const RESPONSE = "10000000-0000-4000-8000-000000000001";
const NEXT_RESPONSE = "10000000-0000-4000-8000-000000000002";
const USER = "20000000-0000-4000-8000-000000000001";
const ASSISTANT = "20000000-0000-4000-8000-000000000002";
const TOOL_OUTPUT = "20000000-0000-4000-8000-000000000003";
const MARKER = "30000000-0000-4000-8000-000000000001";
const NEXT_MARKER = "30000000-0000-4000-8000-000000000002";

function marker(overrides: Partial<ChatCompactionMarker> = {}): ChatCompactionMarker {
  return {
    id: MARKER,
    type: "compaction",
    status: "completed",
    after_item_id: null,
    response_id: RESPONSE,
    created_at: 1792700000,
    generation: 1,
    ...overrides
  };
}

function page(items: unknown[] = [marker()]) {
  return {
    object: "list",
    data: items.map((item) => ({ ...(item as object), object: "conversation.compaction" })),
    has_more: false,
    first_id: items.length ? (items[0] as { id: string }).id : null,
    last_id: items.length ? (items.at(-1) as { id: string }).id : null
  };
}

describe("compaction metadata", () => {
  test("keeps only presentation metadata, discarding opaque content and unknown fields", () => {
    const parsed = parseCompactionPage(
      page([{ ...marker(), encrypted_content: "private-checkpoint", summary: "private-summary" }])
    );
    expect(parsed.data).toEqual([marker()]);
    expect(JSON.stringify(parsed)).not.toContain("private");
  });

  test("accepts an empty terminal page", () => {
    expect(parseCompactionPage(page([]))).toEqual({
      data: [],
      has_more: false,
      first_id: null,
      last_id: null
    });
  });

  test("rejects malformed metadata with a static error that never reflects payload content", () => {
    for (const value of [
      null,
      [],
      { ...page(), object: "not-a-list" },
      page([{ ...marker(), id: "private-invalid-id" }]),
      page([{ ...marker(), after_item_id: "private-invalid-anchor" }]),
      page([{ ...marker(), response_id: "private-invalid-response" }]),
      page([{ ...marker(), status: "in_progress" }]),
      page([{ ...marker(), created_at: -1 }]),
      page([{ ...marker(), created_at: Number.POSITIVE_INFINITY }]),
      page([{ ...marker(), generation: 0 }]),
      page([{ ...marker(), generation: 1.5 }])
    ]) {
      expect(() => parseCompactionPage(value)).toThrow("Invalid conversation compaction metadata");
      try {
        parseCompactionPage(value);
      } catch (error) {
        expect(String(error)).not.toContain("private");
      }
    }
  });

  test("rejects duplicate IDs, oversized pages, and cursor shapes that cannot progress", () => {
    for (const value of [
      page([marker(), marker()]),
      page(Array.from({ length: 101 }, () => marker())),
      { ...page([]), has_more: true },
      { ...page(), last_id: NEXT_MARKER },
      { ...page(), first_id: null }
    ]) {
      expect(() => parseCompactionPage(value)).toThrow();
    }
  });

  test("deduplicates checkpoint refreshes while preserving known ordering and references", () => {
    const first = marker();
    const refreshed = marker({ created_at: 1792700001 });
    const second = marker({ id: NEXT_MARKER, generation: 2 });
    const current = [first];
    const merged = mergeChatCompactionMarkers(current, [refreshed, second]);
    expect(merged).toEqual([refreshed, second]);
    expect(merged[0]).toBe(refreshed);
    expect(current).toEqual([first]);
  });

  test("merges older metadata pages into generation order independent of timestamps", () => {
    const first = marker();
    const second = marker({ id: NEXT_MARKER, generation: 2 });
    expect(mergeChatCompactionMarkers([second], [first])).toEqual([first, second]);
  });
});

describe("compaction timeline projection", () => {
  const user = { id: USER, type: "message", role: "user", response_id: RESPONSE };
  const assistant = {
    id: ASSISTANT,
    type: "message",
    role: "assistant",
    response_id: RESPONSE
  };

  test("places initial preparation after its user turn without changing canonical items", () => {
    const canonical = [user, assistant];
    const checkpoint = marker();
    const projected = projectChatCompactions(canonical, [checkpoint]);
    expect(projected).toEqual([user, checkpoint, assistant]);
    expect(projected[0]).toBe(user);
    expect(projected[2]).toBe(assistant);
    expect(canonical).toEqual([user, assistant]);
  });

  test("places an active-loop checkpoint after its exact completed tool output", () => {
    const output = { id: TOOL_OUTPUT, type: "tool_output", response_id: RESPONSE };
    const checkpoint = marker({ after_item_id: TOOL_OUTPUT });
    expect(projectChatCompactions([user, output, assistant], [checkpoint])).toEqual([
      user,
      output,
      checkpoint,
      assistant
    ]);
  });

  test("omits an off-page tool anchor until older history is loaded", () => {
    const checkpoint = marker({ after_item_id: TOOL_OUTPUT });
    expect(projectChatCompactions([assistant], [checkpoint])).toEqual([assistant]);
    const output = { id: TOOL_OUTPUT, type: "tool_output", response_id: RESPONSE };
    expect(projectChatCompactions([output, assistant], [checkpoint])).toEqual([
      output,
      checkpoint,
      assistant
    ]);
  });

  test("places initial checkpoint before its visible outputs when the user is off-page", () => {
    const checkpoint = marker();
    expect(projectChatCompactions([assistant], [checkpoint])).toEqual([checkpoint, assistant]);
    // Loading the user moves the same marker to its exact initial-turn location.
    expect(projectChatCompactions([user, assistant], [checkpoint])).toEqual([
      user,
      checkpoint,
      assistant
    ]);
  });

  test("does not guess positions for a different response or an empty history", () => {
    const unrelated = marker({ response_id: NEXT_RESPONSE });
    expect(projectChatCompactions([user, assistant], [unrelated])).toEqual([user, assistant]);
    expect(projectChatCompactions([], [marker()])).toEqual([]);
  });

  test("does not duplicate repeated stream/list metadata or already-projected markers", () => {
    const checkpoint = marker();
    const projected = projectChatCompactions([user, assistant], [checkpoint, checkpoint]);
    expect(projected).toEqual([user, checkpoint, assistant]);
    expect(projectChatCompactions(projected, [checkpoint])).toEqual(projected);
  });

  test("preserves multiple checkpoint ordering and canonical item IDs for pagination", () => {
    const first = marker();
    const second = marker({ id: NEXT_MARKER, generation: 2 });
    const canonical = [user, assistant];
    const projected = projectChatCompactions(canonical, [second, first]);
    expect(projected).toEqual([user, first, second, assistant]);
    expect(projected.filter((item) => item.type !== "compaction").map((item) => item.id)).toEqual([
      USER,
      ASSISTANT
    ]);
  });
});

describe("streamed compaction progress", () => {
  const progress = {
    type: "response.compaction.compacting",
    sequence_number: 2,
    output_index: 0,
    item_id: MARKER
  };

  test("recognizes progress before any durable item without retaining private payloads", () => {
    expect(
      chatCompactionProgressUpdate({
        ...progress,
        encrypted_content: "private-checkpoint",
        summary: "private-summary"
      })
    ).toBe(true);
    expect(projectChatCompactions([{ id: USER, type: "message" }], [])).toEqual([
      { id: USER, type: "message" }
    ]);
  });

  test("ignores malformed progress rather than reflecting or persisting its contents", () => {
    for (const event of [
      { ...progress, item_id: "private-invalid-id" },
      { ...progress, item_id: undefined },
      { ...progress, sequence_number: -1 },
      { ...progress, sequence_number: 0.5 },
      { ...progress, sequence_number: Number.POSITIVE_INFINITY },
      { ...progress, output_index: -1 },
      { ...progress, output_index: undefined }
    ]) {
      expect(chatCompactionProgressUpdate(event)).toBeUndefined();
    }
  });

  test("does not require progress from old servers or clear it for unknown events", () => {
    for (const event of [
      null,
      {},
      { type: "response.in_progress" },
      { type: "response.future_event" },
      { type: "response.output_text.delta", delta: "" },
      { type: "response.reasoning_text.delta", delta: "" }
    ]) {
      expect(chatCompactionProgressUpdate(event)).toBeUndefined();
    }
  });

  test("clears advisory progress on main output, durable checkpoint, tools, or terminal", () => {
    for (const event of [
      { type: "response.created" },
      { type: "response.output_item.added", item: { type: "message", id: ASSISTANT } },
      { type: "response.output_item.done", item: { type: "compaction", id: MARKER } },
      { type: "response.output_text.delta", delta: "answer" },
      { type: "response.reasoning_text.delta", delta: "reasoning" },
      { type: "response.content_part.added" },
      { type: "tool_call.created" },
      { type: "tool_output.created" },
      { type: "response.completed", response: { output: [] } },
      { type: "response.failed" },
      { type: "response.cancelled" },
      { type: "error" }
    ]) {
      expect(chatCompactionProgressUpdate(event)).toBe(false);
    }
  });

  test("fallback can complete without manufacturing a durable compaction divider", () => {
    let compacting = false;
    const canonical = [{ id: USER, type: "message" }];
    for (const event of [progress, { type: "response.completed", response: { output: [] } }]) {
      compacting = chatCompactionProgressUpdate(event) ?? compacting;
    }
    expect(compacting).toBe(false);
    expect(projectChatCompactions(canonical, [])).toEqual(canonical);
  });
});
