// Mostly borrowed from https://github.com/ChatGPTNextWeb/ChatGPT-Next-Web/blob/b6bb1673d469a05ddda97a27730e51ae7a231187/app/components/markdown.tsx#L281
import ReactMarkdown from "react-markdown";
import "katex/dist/katex.min.css";
import RemarkMath from "remark-math";
import RemarkBreaks from "remark-breaks";
import RehypeKatex from "rehype-katex";
import RemarkGfm from "remark-gfm";
import RehypeHighlight from "rehype-highlight";
import RehypeSanitize, { defaultSchema } from "rehype-sanitize";
import { useRef, useState, RefObject, useEffect, useMemo } from "react";
import React from "react";
import { Button } from "./ui/button";
import { Check, Copy, ChevronDown, ChevronRight, Brain, FileText } from "lucide-react";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "./ui/dialog";
import { openExternalUrlWithConfirmation } from "@/utils/openUrl";
import { cn } from "@/utils/utils";

async function copyToClipboard(text: string) {
  try {
    await navigator.clipboard.writeText(text);
  } catch (error) {
    console.error("Failed to copy text: ", error);
  }
}

function ThinkingShimmer() {
  return (
    <span className="thinking-shimmer">
      Thinking
      <span className="thinking-shimmer-highlight" aria-hidden="true">
        Thinking
      </span>
    </span>
  );
}

export interface ThinkingBlockProps {
  content: string;
  isThinking: boolean;
  duration?: number;
}

export function ThinkingBlock({ content, isThinking, duration }: ThinkingBlockProps) {
  const [isExpanded, setIsExpanded] = useState(false);

  // Use the reported duration when available, otherwise estimate completed thoughts by word count.
  const displayDuration = useMemo(() => {
    if (duration) return duration;

    // Fallback: estimate based on word count
    const wordCount = content.trim().split(/\s+/).length;
    // Slower estimation to better match observed times (26 actual vs 23 estimated)
    const estimatedSeconds = Math.max(1, Math.round(wordCount / 26)); // ~26 words per second
    return estimatedSeconds;
  }, [content, duration]);

  const durationText = `${displayDuration}`;

  return (
    <div className="my-3">
      <button
        type="button"
        onClick={() => setIsExpanded(!isExpanded)}
        className="flex w-full items-center gap-2 rounded-md py-2 text-left"
        aria-expanded={isExpanded}
      >
        <Brain className="h-4 w-4 shrink-0 text-muted-foreground" />
        <span className="flex min-w-0 flex-1 items-center gap-2">
          <span className="min-w-0 text-sm font-medium text-muted-foreground">
            {isThinking ? <ThinkingShimmer /> : `Thought for ${durationText} seconds`}
          </span>
          <span className="shrink-0 text-muted-foreground" aria-hidden>
            {isExpanded ? (
              <ChevronDown className="h-4 w-4" />
            ) : (
              <ChevronRight className="h-4 w-4" />
            )}
          </span>
        </span>
      </button>
      {isExpanded && (
        <div className="pb-1 pt-2">
          <div className="whitespace-pre-wrap text-sm text-muted-foreground">{content}</div>
        </div>
      )}
    </div>
  );
}

interface ParsedContent {
  type: "thinking" | "content";
  content: string;
  duration?: string; // Store duration as string like "23"
  id?: string; // Unique ID to track thinking blocks across renders
}

function parseThinkingTags(content: string, isComplete: boolean = false): ParsedContent[] {
  const parts: ParsedContent[] = [];

  // Check for the edge case: <think> followed by only whitespace and no closing tag
  if (isComplete && /^<think>\s*$/m.test(content) && !content.includes("</think>")) {
    // Strip out the incomplete <think> tag and treat everything after as content
    const strippedContent = content.replace(/^<think>\s*/m, "");
    if (strippedContent.trim()) {
      parts.push({ type: "content", content: strippedContent });
    }
    return parts;
  }

  // Pattern to match <think> tags (complete or incomplete)
  // During streaming (!isComplete), we want to catch <think> as soon as it appears
  const thinkPattern = isComplete
    ? /<think>([\s\S]*?)<\/think>|<think>([\s\S]*?)$/g
    : /<think>([\s\S]*?)(?:<\/think>|$)/g;

  let lastIndex = 0;
  let match;

  while ((match = thinkPattern.exec(content)) !== null) {
    // Add content before the think tag
    if (match.index > lastIndex) {
      const beforeContent = content.slice(lastIndex, match.index);
      if (beforeContent.trim()) {
        parts.push({ type: "content", content: beforeContent });
      }
    }

    // Extract content from the match
    const thinkContent = match[1] ?? match[2] ?? "";

    // During streaming, even empty think tags should be shown to indicate thinking is starting
    if (!isComplete && match[0].includes("<think>")) {
      parts.push({
        type: "thinking",
        content: thinkContent,
        duration: undefined,
        id: `think-${match.index}`
      });
    } else if (thinkContent.trim()) {
      // For complete content, only add if there's actual content
      parts.push({
        type: "thinking",
        content: thinkContent,
        duration: undefined,
        id: `think-${match.index}`
      });
    }

    lastIndex = match.index + match[0].length;
  }

  // Add any remaining content
  if (lastIndex < content.length) {
    const remainingContent = content.slice(lastIndex);
    if (remainingContent.trim()) {
      parts.push({ type: "content", content: remainingContent });
    }
  }

  return parts;
}

function PreCode(props: JSX.IntrinsicElements["pre"]) {
  const ref = useRef<HTMLPreElement>(null);

  const [isCopied, setIsCopied] = useState(false);

  //Wrap the paragraph for plain-text
  useEffect(() => {
    if (ref.current) {
      const codeElements = ref.current.querySelectorAll("code") as NodeListOf<HTMLElement>;
      const wrapLanguages = ["", "md", "markdown", "text", "txt", "plaintext", "tex", "latex"];
      codeElements.forEach((codeElement) => {
        const languageClass = codeElement.className.match(/language-(\w+)/);
        const name = languageClass ? languageClass[1] : "";
        if (wrapLanguages.includes(name)) {
          codeElement.style.whiteSpace = "pre-wrap";
        }
      });
    }
  }, []);

  return (
    <div className="w-full">
      <div className="flex justify-end pt-2 mb-1">
        <Button
          variant="ghost"
          size="sm"
          className="gap-2"
          onClick={() => {
            if (ref.current) {
              const code = ref.current.innerText;
              copyToClipboard(code);
              setIsCopied(true);
              setTimeout(() => setIsCopied(false), 2000);
            }
          }}
          aria-label={isCopied ? "Copied" : "Copy to clipboard"}
        >
          {isCopied ? (
            <>
              <Check className="h-4 w-4" />
              <span className="font-sans text-sm">Copied</span>
            </>
          ) : (
            <>
              <Copy className="h-4 w-4" />
              <span className="font-sans text-sm">Copy Code</span>
            </>
          )}
        </Button>
      </div>
      <pre ref={ref} className="w-full overflow-x-auto">
        {props.children}
      </pre>
    </div>
  );
}

function CustomCode(props: JSX.IntrinsicElements["code"]) {
  return <code>{props.children}</code>;
}

function ResponsiveTable({ children, className, ...rest }: JSX.IntrinsicElements["table"]) {
  // Strip off props added by react-markdown that the DOM doesn't understand
  // eslint-disable-next-line @typescript-eslint/no-unused-vars
  const { node, inline, ...safeRest } = rest as Record<string, unknown>;
  const tableProps = safeRest as JSX.IntrinsicElements["table"];
  const tableStyle = {
    ...(tableProps.style ?? {}),
    minWidth: "max-content"
  };

  const scrollRef = useRef<HTMLDivElement>(null);
  const [canScrollLeft, setCanScrollLeft] = useState(false);
  const [canScrollRight, setCanScrollRight] = useState(false);

  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;

    const checkScroll = () => {
      const { scrollLeft, scrollWidth, clientWidth } = el;
      setCanScrollLeft(scrollLeft > 0);
      setCanScrollRight(scrollLeft + clientWidth < scrollWidth - 1);
    };

    const frameId = window.requestAnimationFrame(checkScroll);

    el.addEventListener("scroll", checkScroll, { passive: true });
    window.addEventListener("resize", checkScroll);

    let resizeObserver: ResizeObserver | undefined;
    if (typeof ResizeObserver !== "undefined") {
      resizeObserver = new ResizeObserver(checkScroll);
      resizeObserver.observe(el);
      const tableEl = el.querySelector("table");
      if (tableEl) {
        resizeObserver.observe(tableEl);
      }
    }

    return () => {
      window.cancelAnimationFrame(frameId);
      el.removeEventListener("scroll", checkScroll);
      window.removeEventListener("resize", checkScroll);
      resizeObserver?.disconnect();
    };
  }, [children]);

  return (
    <div className="relative my-4 -mx-4 px-4">
      {canScrollLeft && (
        <div className="pointer-events-none absolute bottom-0 left-[15px] top-0 z-10 w-8 bg-gradient-to-r from-background via-background/85 to-transparent" />
      )}
      {canScrollRight && (
        <div className="pointer-events-none absolute bottom-0 right-[15px] top-0 z-10 w-8 bg-gradient-to-l from-background via-background/85 to-transparent" />
      )}
      <div
        ref={scrollRef}
        className={cn(
          "maple-thin-scrollbar overflow-x-auto overflow-y-visible overscroll-x-contain",
          "[-webkit-overflow-scrolling:touch]"
        )}
      >
        <table
          {...tableProps}
          className={cn("markdown-table-maple", className || "")}
          style={tableStyle}
        >
          {children}
        </table>
      </div>
    </div>
  );
}

function escapeDollarNumber(text: string) {
  let escapedText = "";

  for (let i = 0; i < text.length; i += 1) {
    let char = text[i];
    const nextChar = text[i + 1] || " ";

    if (char === "$" && nextChar >= "0" && nextChar <= "9") {
      char = "\\$";
    }

    escapedText += char;
  }

  return escapedText;
}

function escapeBrackets(text: string): string {
  // First, handle code blocks to protect them from processing
  const codeBlockPattern = /```[\s\S]*?```|`.*?`/g;
  const codeBlocks: string[] = [];
  const textWithoutCode = text.replace(codeBlockPattern, function (match: string) {
    codeBlocks.push(match);
    return `__CODE_BLOCK_${codeBlocks.length - 1}__`;
  });

  // Process escaped square brackets - LaTeX display mode
  const squareBracketPattern = /\\\[([\s\S]*?[^\\])\\\]/g;
  let result = textWithoutCode.replace(
    squareBracketPattern,
    function (_match: string, content: string) {
      return `$$${content}$$`;
    }
  );

  // Process escaped parentheses - LaTeX inline mode
  const roundBracketPattern = /\\\((.*?)\\\)/g;
  result = result.replace(roundBracketPattern, function (_match: string, content: string) {
    return `$${content}$`;
  });

  // Restore code blocks
  codeBlocks.forEach(function (block: string, i: number) {
    result = result.replace(`__CODE_BLOCK_${i}__`, block);
  });

  return result;
}

// Disable Markdown's "4-space indent = code block" rule. Model output frequently
// indents wrapped lines or list continuations by 4 spaces, which CommonMark/GFM
// turn into an indented code block — so plain text renders inside a <pre><code>.
// Fenced ``` / ~~~ blocks are unaffected; only the accidental-indentation path is
// removed.
function remarkNoIndentedCode(this: { data: (key?: string) => Record<string, unknown> }) {
  const data = this.data() as { micromarkExtensions?: unknown[] };
  const micromarkExtensions = (data.micromarkExtensions ||
    (data.micromarkExtensions = [])) as unknown[];
  micromarkExtensions.push({ disable: { null: ["codeIndented"] } });
}

function MarkDownContentToMemo(props: { content: string }) {
  const escapedContent = useMemo(() => {
    return escapeBrackets(escapeDollarNumber(props.content));
  }, [props.content]);

  return (
    <ReactMarkdown
      remarkPlugins={[RemarkMath, RemarkGfm, RemarkBreaks, remarkNoIndentedCode]}
      rehypePlugins={[
        [
          RehypeSanitize,
          {
            ...defaultSchema,
            attributes: {
              ...defaultSchema.attributes,
              code: [
                ...(defaultSchema.attributes?.code || []),
                ["className", /^language-./, "math-inline", "math-display"]
              ],
              span: [...(defaultSchema.attributes?.span || []), ["className", /^katex/]],
              div: [...(defaultSchema.attributes?.div || []), ["className", "katex-display"]]
            }
          }
        ],
        RehypeKatex,
        [
          RehypeHighlight,
          {
            detect: false,
            ignoreMissing: true
          }
        ]
      ]}
      components={{
        // Uploaded images use structured input_image parts outside Markdown. Preserve
        // useful alt text here without creating an image element or loading its URL.
        img: ({ alt }) => (alt ? <span>{alt}</span> : null),
        pre: (props: JSX.IntrinsicElements["pre"]) => <PreCode {...props} />,
        code: (props: JSX.IntrinsicElements["code"]) => <CustomCode {...props} />,
        table: (props: JSX.IntrinsicElements["table"]) => <ResponsiveTable {...props} />,
        p: (pProps) => <p {...pProps} dir="auto" />,
        a: (aProps) => {
          const href = aProps.href || "";
          const isInternal = /^\/#/i.test(href);
          const target = isInternal ? "_self" : (aProps.target ?? "_blank");

          // For external links, show confirmation before opening
          if (!isInternal && href) {
            return (
              <a
                {...aProps}
                href={href}
                target={target}
                onClick={(e) => {
                  e.preventDefault();
                  openExternalUrlWithConfirmation(href);
                }}
                style={{ cursor: "pointer" }}
              />
            );
          }

          return <a {...aProps} target={target} />;
        }
      }}
    >
      {escapedContent}
    </ReactMarkdown>
  );
}

export const MarkdownContent = React.memo(MarkDownContentToMemo);

interface DocumentData {
  document: {
    filename: string;
    text_content: string | null;
  };
}

// Type guard to validate DocumentData structure
function isDocumentData(obj: unknown): obj is DocumentData {
  if (!obj || typeof obj !== "object") return false;

  const data = obj as Record<string, unknown>;

  // Check top-level properties
  if (!("document" in data)) {
    return false;
  }

  // Check document object structure
  const doc = data.document;
  if (!doc || typeof doc !== "object") return false;

  const docObj = doc as Record<string, unknown>;

  // Check required document properties
  if (!("filename" in docObj) || typeof docObj.filename !== "string") return false;

  // Require text_content (must be string)
  if (!("text_content" in docObj) || typeof docObj.text_content !== "string") return false;

  return true;
}

function DocumentPreview({ documentData }: { documentData: DocumentData }) {
  const [isOpen, setIsOpen] = useState(false);

  // Extract content
  const content = documentData.document.text_content || "No content available";

  return (
    <>
      <div className="my-3">
        <Button
          variant="outline"
          size="default"
          className="h-20 w-20 p-2 flex flex-col items-center justify-center gap-1 overflow-hidden"
          onClick={() => setIsOpen(true)}
          title={documentData.document.filename}
          aria-label={`Preview document: ${documentData.document.filename}`}
        >
          <FileText className="h-6 w-6 flex-shrink-0" />
          <span className="text-xs truncate w-full text-center px-1">
            {documentData.document.filename}
          </span>
        </Button>
      </div>

      <Dialog open={isOpen} onOpenChange={setIsOpen}>
        <DialogContent className="max-w-4xl max-h-[80vh] overflow-hidden flex flex-col">
          <DialogHeader>
            <DialogTitle>{documentData.document.filename}</DialogTitle>
          </DialogHeader>
          <div className="overflow-y-auto flex-1 p-4">
            <pre className="whitespace-pre-wrap font-sans text-sm leading-relaxed">{content}</pre>
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}

function parseDocumentJson(text: string): { data: DocumentData; endIndex: number } | null {
  const start = text.indexOf('{"document":');
  if (start === -1) return null;

  // Try to find a complete JSON object using a more robust approach
  // We'll attempt to parse progressively larger substrings
  const jsonStart = text.substring(start);

  // First, try to parse the entire remaining string
  try {
    const parsed = JSON.parse(jsonStart);
    // Validate the structure using our type guard
    if (isDocumentData(parsed)) {
      return { data: parsed, endIndex: start + jsonStart.length };
    }
  } catch {
    // If full parse fails, we need to find the end of the JSON object
  }

  // Use a state machine approach to properly handle strings and escapes
  let inString = false;
  let escapeNext = false;
  let depth = 0;

  for (let i = 0; i < jsonStart.length; i++) {
    const ch = jsonStart[i];

    if (escapeNext) {
      escapeNext = false;
      continue;
    }

    if (ch === "\\") {
      escapeNext = true;
      continue;
    }

    if (ch === '"' && !escapeNext) {
      inString = !inString;
      continue;
    }

    if (!inString) {
      if (ch === "{") depth++;
      else if (ch === "}") {
        depth--;
        if (depth === 0) {
          try {
            const candidate = jsonStart.substring(0, i + 1);
            const parsed = JSON.parse(candidate);
            // Validate the structure using our type guard
            if (isDocumentData(parsed)) {
              return { data: parsed, endIndex: start + i + 1 };
            }
          } catch (e) {
            // Continue searching if this wasn't valid JSON
            console.error("Document JSON parse error at position", i, e);
          }
        }
      }
    }
  }

  return null; // incomplete JSON – wait for more chunks
}

function parseContentWithDocuments(
  content: string
): Array<{ type: "text" | "document"; content: string | DocumentData }> {
  const parts: Array<{ type: "text" | "document"; content: string | DocumentData }> = [];

  // Check if content contains document JSON
  if (content.includes('{"document":')) {
    const jsonStartIndex = content.indexOf('{"document":');
    const beforeJson = content.substring(0, jsonStartIndex).trim();

    // Add any text before the JSON
    if (beforeJson) {
      parts.push({ type: "text", content: beforeJson });
    }

    // Try to parse the document JSON
    const parseResult = parseDocumentJson(content);
    if (parseResult) {
      parts.push({ type: "document", content: parseResult.data });

      // Find any text after the JSON using the endIndex from parsing
      const afterJson = content.substring(parseResult.endIndex).trim();
      if (afterJson) {
        parts.push({ type: "text", content: afterJson });
      }
    } else {
      // If parsing failed, just show as text
      parts.push({ type: "text", content: content });
    }
  } else {
    // No document detected, treat as regular text
    parts.push({ type: "text", content: content });
  }

  return parts;
}

function MarkdownWithThinking({
  content,
  loading = false,
  chatId
}: {
  content: string;
  loading?: boolean;
  chatId?: string;
}) {
  const parsedContent = useMemo(() => {
    // Pass isComplete (which is !loading) to handle the edge case
    return parseThinkingTags(content, !loading);
  }, [content, loading]);

  return (
    <>
      {parsedContent.map((part, index) => {
        if (part.type === "thinking") {
          // Check if this thinking block is still being streamed
          const isLastPart = index === parsedContent.length - 1;
          // During streaming, check if this thinking block doesn't have a closing tag
          const thisThinkingPosition = content.lastIndexOf("<think>");
          const closingPosition = content.lastIndexOf("</think>");

          // It's actively thinking if we're loading and this think tag hasn't been closed yet
          const isThinking = loading && isLastPart && closingPosition < thisThinkingPosition;

          return (
            <ThinkingBlock
              key={`${chatId}-${part.id || index}`} // Include chatId in key to reset state
              content={part.content}
              isThinking={isThinking}
              duration={undefined} // Let ThinkingBlock handle duration internally
            />
          );
        } else {
          // Parse content for documents
          const contentParts = parseContentWithDocuments(part.content);
          return (
            <React.Fragment key={index}>
              {contentParts.map((contentPart, partIndex) => {
                if (contentPart.type === "document") {
                  return (
                    <DocumentPreview
                      key={`doc-${index}-${partIndex}`}
                      documentData={contentPart.content as DocumentData}
                    />
                  );
                } else {
                  return (
                    <MarkdownContent
                      key={`text-${index}-${partIndex}`}
                      content={contentPart.content as string}
                    />
                  );
                }
              })}
            </React.Fragment>
          );
        }
      })}
    </>
  );
}

export function Markdown(
  props: {
    content: string;
    loading?: boolean;
    parentRef?: RefObject<HTMLDivElement>;
    defaultShow?: boolean;
    chatId?: string;
  } & React.DOMAttributes<HTMLDivElement>
) {
  const mdRef = useRef<HTMLDivElement>(null);

  return (
    <div
      className="markdown-body"
      style={{
        wordBreak: "break-word"
      }}
      ref={mdRef}
      onContextMenu={props.onContextMenu}
      onDoubleClickCapture={props.onDoubleClickCapture}
      dir="auto"
    >
      <MarkdownWithThinking content={props.content} loading={props.loading} chatId={props.chatId} />
    </div>
  );
}
