import { memo, useEffect, useState, type ReactNode } from "react";
import ReactMarkdown, { defaultUrlTransform, type Options } from "react-markdown";
import remarkGfm from "remark-gfm";
import { codeToHtml } from "shiki";
import { api } from "../lib/api";
import { classifyHref, filePathsRehype, isPathLike } from "../lib/fileLinks";
import { cn } from "../lib/cn";
import { FilePathRef, InsideRefContext } from "./FilePathRef";

// Script-y schemes are never handed to the DOM href or the OS opener.
const UNSAFE_HREF = /^\s*(?:javascript|data|vbscript):/i;

const CODE_CHIP = "rounded bg-raised px-1 py-0.5 font-mono text-[11.5px] text-ink";

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

function parseLanguage(className?: string): string {
  const match = /language-(\w+)/.exec(String(className ?? ""));
  return match?.[1] ?? "text";
}

function currentShikiTheme(): "github-dark" | "github-light" {
  return document.documentElement.dataset.theme === "light" ? "github-light" : "github-dark";
}

const htmlCache = new Map<string, string>();

/**
 * AI SDK Elements-style CodeBlock primitive, self-contained and Weft-skinned.
 * Highlights fenced code with shiki and preserves the surrounding markdown
 * link/file-ref semantics.
 */
function CodeBlock({
  code,
  language,
  className,
}: {
  code: string;
  language: string;
  className?: string;
}) {
  const [html, setHtml] = useState<string>(() => htmlCache.get(`${language}:${code}`) ?? "");

  useEffect(() => {
    const key = `${language}:${code}`;
    const cached = htmlCache.get(key);
    if (cached) {
      setHtml(cached);
      return;
    }
    let cancelled = false;
    const run = async () => {
      const theme = currentShikiTheme();
      try {
        const out = await codeToHtml(code, { lang: language, theme });
        if (cancelled) return;
        htmlCache.set(key, out);
        setHtml(out);
      } catch {
        // Unknown language: fall back to plain-text highlighting.
        const out = await codeToHtml(code, { lang: "text", theme });
        if (cancelled) return;
        htmlCache.set(key, out);
        setHtml(out);
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [code, language]);

  return (
    <pre
      className={cn(
        "overflow-x-auto rounded-[var(--radius-md)] border border-border bg-raised p-3 text-[11.5px]",
        className,
      )}
    >
      {html ? (
        <code
          className="font-mono"
          dangerouslySetInnerHTML={{ __html: html }}
        />
      ) : (
        <code className="font-mono">{code}</code>
      )}
    </pre>
  );
}

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
            if (block) {
              return <CodeBlock code={content} language={parseLanguage(className)} className="my-2" />;
            }
            if (isPathLike(content, true)) {
              return (
                <FilePathRef token={content} cwd={cwd} code isUrl={/^file:/i.test(content)}>
                  {children}
                </FilePathRef>
              );
            }
            return <code className={CODE_CHIP}>{children}</code>;
          },
          span: ({ node, children }) => {
            const fp = (node as { properties?: Record<string, unknown> } | undefined)?.properties
              ?.dataFilepath;
            if (typeof fp === "string") {
              return (
                <FilePathRef token={fp} cwd={cwd} isUrl={/^file:/i.test(fp)}>
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
