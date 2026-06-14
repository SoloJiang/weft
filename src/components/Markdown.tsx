import { memo, type ReactNode } from "react";
import ReactMarkdown, { defaultUrlTransform, type Options } from "react-markdown";
import remarkGfm from "remark-gfm";
import { api } from "../lib/api";
import { classifyHref, filePathsRehype, isPathLike } from "../lib/fileLinks";
import { FilePathRef } from "./FilePathRef";

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
export const Markdown = memo(function Markdown({ text, cwd }: { text: string; cwd?: string }) {
  return (
    <div className="weft-md text-[12.5px] leading-relaxed text-ink">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[filePathsRehype] as unknown as Options["rehypePlugins"]}
        urlTransform={fileAwareUrlTransform}
        components={{
          a: ({ href, children }) => {
            if (!href) return <>{children}</>;
            const c = classifyHref(href);
            if (c.kind === "file") {
              return (
                <FilePathRef token={c.token} cwd={cwd}>
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
                {children}
              </a>
            );
          },
          code: ({ className, children }) => {
            const inline = !String(className ?? "").includes("language-");
            if (!inline) return <code className="font-mono text-[11.5px]">{children}</code>;
            const content = nodeText(children);
            if (isPathLike(content)) {
              return (
                <FilePathRef token={content} cwd={cwd} code>
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
            const fp = (node as { properties?: Record<string, unknown> } | undefined)?.properties
              ?.dataFilepath;
            if (typeof fp === "string") {
              return (
                <FilePathRef token={fp} cwd={cwd}>
                  {children}
                </FilePathRef>
              );
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
