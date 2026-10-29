export type ToolKind = "shell" | "file-read" | "file-write" | "web" | "mcp" | "generic";
export type ToolActivityStatus = "active" | "completed" | "incomplete" | "error";

const SHELL_TOOL_NAMES = new Set(["shell"]);
const FILE_READ_TOOL_NAMES = new Set([
  "read",
  "read_file",
  "read_image",
  "list_files",
  "glob",
  "grep"
]);
const FILE_WRITE_TOOL_NAMES = new Set([
  "edit",
  "write",
  "edit_file",
  "write_file",
  "text_editor",
  "str_replace_editor",
  "str_replace_based_edit_tool",
  "apply_patch"
]);
const WEB_TOOL_NAMES = new Set(["web_search", "open_url", "open_urls"]);
const MAPLE_EXTENSION_NAMES = new Set(["developer"]);

export function toolKindFromName(encodedName: string): ToolKind {
  const namespaceSeparator = encodedName.indexOf("__");
  const namespace = namespaceSeparator >= 0 ? encodedName.slice(0, namespaceSeparator) : null;
  const toolName =
    namespaceSeparator >= 0 ? encodedName.slice(namespaceSeparator + 2) : encodedName;

  if (namespace && !MAPLE_EXTENSION_NAMES.has(namespace)) return "mcp";
  if (SHELL_TOOL_NAMES.has(toolName)) return "shell";
  if (FILE_READ_TOOL_NAMES.has(toolName)) return "file-read";
  if (FILE_WRITE_TOOL_NAMES.has(toolName)) return "file-write";
  if (WEB_TOOL_NAMES.has(toolName)) return "web";
  return "generic";
}

export function toolKindLabel(kind: ToolKind): string {
  switch (kind) {
    case "shell":
      return "Shell command";
    case "file-read":
      return "File read";
    case "file-write":
      return "File change";
    case "web":
      return "Web tool";
    case "mcp":
      return "MCP tool";
    default:
      return "Tool call";
  }
}
