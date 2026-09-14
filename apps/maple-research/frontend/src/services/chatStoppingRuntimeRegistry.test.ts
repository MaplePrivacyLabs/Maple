import { describe, expect, test } from "bun:test";
import { chatStoppingRuntimeRegistryFor } from "./chatStoppingRuntimeRegistry";
import type { ChatRuntimeKey } from "./chatRuntimeStore";

describe("chat stopping runtime registry", () => {
  test("clears a Stop token after its run rekeys from draft to conversation", () => {
    const registry = chatStoppingRuntimeRegistryFor({});
    const draftKey = "draft:one" as ChatRuntimeKey;
    const conversationKey = "conversation:one" as ChatRuntimeKey;

    registry.add(draftKey, 7);
    registry.delete(conversationKey, 7);

    expect(registry.getEntries().size).toBe(0);
  });

  test("deleting one run leaves other Stop tokens intact", () => {
    const registry = chatStoppingRuntimeRegistryFor({});
    const firstKey = "draft:first" as ChatRuntimeKey;
    const secondKey = "draft:second" as ChatRuntimeKey;

    registry.add(firstKey, 1);
    registry.add(secondKey, 2);
    registry.delete(firstKey, 1);

    expect(registry.getEntries().get(firstKey)).toBeUndefined();
    expect(registry.getEntries().get(secondKey)).toEqual(new Set([2]));
  });

  test("publishes only for effective mutations", () => {
    const registry = chatStoppingRuntimeRegistryFor({});
    const key = "draft:one" as ChatRuntimeKey;
    let notifications = 0;
    registry.subscribe(() => {
      notifications += 1;
    });

    registry.add(key, 3);
    registry.add(key, 3);
    registry.delete(key, 99);
    registry.delete(key, 3);

    expect(notifications).toBe(2);
  });
});
