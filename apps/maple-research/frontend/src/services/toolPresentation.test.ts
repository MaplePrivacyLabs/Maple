import { describe, expect, test } from "bun:test";
import { toolKindFromName, toolKindLabel } from "./toolPresentation";

describe("toolKindFromName", () => {
  test.each([
    ["shell", "shell"],
    ["developer__shell", "shell"],
    ["read_file", "file-read"],
    ["developer__read_image", "file-read"],
    ["edit_file", "file-write"],
    ["write_file", "file-write"],
    ["web_search", "web"],
    ["developer__open_url", "web"],
    ["open_urls", "web"],
    ["github__search_code", "mcp"],
    ["skills__load_skill", "mcp"],
    ["skills__shell", "mcp"],
    ["unknown_tool", "generic"]
  ] as const)("classifies %s as %s", (name, expected) => {
    expect(toolKindFromName(name)).toBe(expected);
  });
});

describe("toolKindLabel", () => {
  test("provides a text alternative for every visual type", () => {
    expect(toolKindLabel("shell")).toBe("Shell command");
    expect(toolKindLabel("file-read")).toBe("File read");
    expect(toolKindLabel("file-write")).toBe("File change");
    expect(toolKindLabel("web")).toBe("Web tool");
    expect(toolKindLabel("mcp")).toBe("MCP tool");
    expect(toolKindLabel("generic")).toBe("Tool call");
  });
});
