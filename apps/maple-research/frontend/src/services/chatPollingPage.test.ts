import { describe, expect, test } from "bun:test";
import { normalizeChatPollingPage } from "./chatPollingPage";

const oldest = { id: "oldest", status: "completed" };
const middle = { id: "middle", status: "completed" };
const newest = { id: "newest", status: "in_progress" };

describe("normalizeChatPollingPage", () => {
  test("keeps cursor-based ascending pages chronological", () => {
    const normalized = normalizeChatPollingPage([oldest, middle, newest], true);

    expect(normalized.chronologicalItems.map((item) => item.id)).toEqual([
      "oldest",
      "middle",
      "newest"
    ]);
    expect(normalized.newestCompletedItem?.id).toBe("middle");
  });

  test("reverses a no-cursor descending recovery page before merging", () => {
    const normalized = normalizeChatPollingPage([newest, middle, oldest], false);

    expect(normalized.chronologicalItems.map((item) => item.id)).toEqual([
      "oldest",
      "middle",
      "newest"
    ]);
    expect(normalized.newestCompletedItem?.id).toBe("middle");
  });
});
