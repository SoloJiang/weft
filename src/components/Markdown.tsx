import { memo, type ReactNode } from "react";
import ReactMarkdown, { defaultUrlTransform, type Options } from "react-markdown";
import remarkGfm from "remark-gfm";
import { api } from "../lib/api";
import { classifyHref, filePathsRehype, isPathLike } from "../lib/fileLinks";
import { FilePathRef, InsideRefContext } from "./FilePathRef";

// Script-y schemes are never handed to the DOM href or the OS opener.
const UNSAFE_HREF = /^\s*(?:javascript|data|vbscript):/i;

// react-markdown's default sanitizer blanks any non-web scheme, which would
// strip both local file paths and legitimate app deep links (ms-settings:,
// vscode-insiders://, codex://) before our `a` renderer can route them. We never
// navigate to an href — clicks are preventDefault'd and sent to the OS opener /
// file resolver — so for links we preserve everything except the script-y
// denylist. Image `src` still uses the strict default sanitizer.
const fileAwareUrlTransform: NonNullable<Options["urlTransform"]> = (url, key) => {
  if (key !== "href") return defaultUrlTransform(url);
  return UNSAFE_HREF.test(url) ? "" : url;
};

/**
 * Renders agent output as markdown — headings, lists, code, tables, links —
 * scoped + sized to fit the transcript (no global prose plugin needed). Web
 * links open in the OS browser via the opener; local file paths (in links,
 * inline code, or prose) become quiet file references — ⌘-click to open, right
 * to reveal — resolved against the session's working dir (`cwd`).
 */
/** Streaming caret styling — shared with ChatTimeline's pre-text fallback so the
 *  inline caret and the empty-state caret stay visually identical. */
export const STREAM_CARET_CLASS =
  "ml-0.5 inline-block h-3.5 w-[2px] animate-pulse rounded bg-brand align-text-bottom";

export const Markdown = memo(function Markdown({
  text,
  cwd,
  caret,
}: {
  text: string;
  cwd?: string;
  /** While true, a blinking caret is appended inline after the last character. */
  caret?: boolean;
}) {
  const rehypePlugins = (caret
    ? [filePathsRehype, caretRehype]
    : [filePathsRehype]) as unknown as Options["rehypePlugins"];
  return (
    <div className="weft-md text-[12.5px] leading-relaxed text-ink">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={rehypePlugins}
        urlTransform={fileAwareUrlTransform}
        components={{
          a: ({ href, children }) => {
            if (!href) return <>{children}</>;
            // In-page anchor: let the DOM handle the jump, never the OS opener.
            if (href.startsWith("#")) return <a href={href}>{children}</a>;
            const c = classifyHref(href);
            if (c.kind === "file") {
              return (
                <FilePathRef token={c.token} cwd={cwd} isUrl>
                  {children}
                </FilePathRef>
              );
            }
            return (
              <a
                href={href}
                onClick={(e) => {
                  e.preventDefault();
                  void api.openUrl(c.url);
                }}
                className="text-brand underline decoration-brand/40 underline-offset-2 hover:decoration-brand"
              >
                {/* Inline-code label inside the link stays inert — the <a> owns the click. */}
                <InsideRefContext.Provider value={true}>{children}</InsideRefContext.Provider>
              </a>
            );
          },
          code: ({ className, children }) => {
            const content = nodeText(children);
            // Block code = a language class OR any newline. A language-less fence
            // still carries a trailing \n (verified), so only true single-line
            // inline code is eligible to become a file ref — never a fenced block.
            const block = String(className ?? "").includes("language-") || content.includes("\n");
            if (block) return <code className="font-mono text-[11.5px]">{children}</code>;
            if (isPathLike(content, true)) {
              return (
                <FilePathRef token={content} cwd={cwd} code isUrl={/^file:/i.test(content)}>
                  {children}
                </FilePathRef>
              );
            }
            return (
              <code className="rounded bg-raised px-1 py-0.5 font-mono text-[11.5px] text-ink">
                {children}
              </code>
            );
          },
          span: ({ node, children }) => {
            const props = (node as { properties?: Record<string, unknown> } | undefined)
              ?.properties;
            const fp = props?.dataFilepath;
            if (typeof fp === "string") {
              return (
                <FilePathRef token={fp} cwd={cwd} isUrl={/^file:/i.test(fp)}>
                  {children}
                </FilePathRef>
              );
            }
            if (props?.dataStreamCaret) {
              return <span className={STREAM_CARET_CLASS} aria-hidden />;
            }
            return <span>{children}</span>;
          },
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});

/** Flatten an inline-code node's children to its raw string. */
function nodeText(children: ReactNode): string {
  if (typeof children === "string") return children;
  if (Array.isArray(children)) return children.map(nodeText).join("");
  return "";
}

interface HNode {
  type: string;
  tagName?: string;
  value?: string;
  properties?: Record<string, unknown>;
  children?: HNode[];
}

/**
 * rehype plugin: place a caret marker right after the last visible character so
 * the streaming cursor hugs the text. A plain sibling span would land below the
 * rendered block (markdown emits block elements), so we descend into the deepest
 * trailing element and inject the caret inline there. The `span` override paints it.
 */
function caretRehype() {
  return (tree: unknown) => {
    insertCaret((tree as HNode).children);
  };
}

function insertCaret(nodes: HNode[] | undefined): boolean {
  if (!nodes) return false;
  for (let i = nodes.length - 1; i >= 0; i--) {
    const n = nodes[i];
    if (n.type === "text") {
      if ((n.value ?? "").trim() === "") continue; // skip inter-block whitespace
      nodes.splice(i + 1, 0, caretNode());
      return true;
    }
    if (n.type === "element") {
      if (insertCaret(n.children)) return true;
      nodes.splice(i + 1, 0, caretNode()); // empty/void element → caret right after it
      return true;
    }
  }
  return false;
}

function caretNode(): HNode {
  return { type: "element", tagName: "span", properties: { dataStreamCaret: true }, children: [] };
}
