import {
  createContext,
  Fragment,
  memo,
  useContext,
  useEffect,
  useId,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import { useTranslation } from "react-i18next";
import MarkdownRender, {
  setCustomComponents,
  type CustomComponentMap,
  type NodeComponentProps,
} from "markstream-react";
import "markstream-react/index.css";
import {
  parseMarkdownToStructure,
  type CodeBlockNode,
  type HtmlBlockNode,
  type HtmlInlineNode,
  type InlineCodeNode,
  type LinkNode,
  type ParsedNode,
  type TextNode,
} from "stream-markdown-parser";
import { codeToHtml } from "shiki";
import { api } from "../lib/api";
import { injectCaret, WEFT_CARET_TYPE } from "../lib/markdownCaret";
import { allowHref, createWeftMarkdown, UNSAFE_HREF } from "../lib/markdownParser";
import { classifyHref, isPathLike } from "../lib/fileLinks";
import { hasLineLabelSyntax, splitTextForPaths } from "../lib/filePathParsing";
import { cn } from "../lib/cn";
import { FilePathRef, InsideRefContext } from "./FilePathRef";
import { ShellSnippet } from "./ai-elements";

const CODE_CHIP = "rounded bg-raised px-1 py-0.5 font-mono text-[11.5px] text-ink";

/** Session working dir for resolving relative file paths — provided by the
 *  <Markdown> host, consumed by the module-registered node components. */
const MarkdownCwdContext = createContext<string | undefined>(undefined);

function parseLanguage(language?: string): string {
  const head = String(language ?? "").trim().split(/\s+/)[0] ?? "";
  return head.split(":")[0].toLowerCase() || "text";
}

function isShellLanguage(language: string): boolean {
  return ["bash", "console", "sh", "shell", "terminal", "zsh"].includes(language);
}

function currentShikiTheme(): "github-dark" | "github-light" {
  return document.documentElement.dataset.theme === "light" ? "github-light" : "github-dark";
}

const htmlCache = new Map<string, string>();

function useShikiTheme(): "github-dark" | "github-light" {
  const [theme, setTheme] = useState(currentShikiTheme);

  useEffect(() => {
    const root = document.documentElement;
    const syncTheme = () => setTheme(currentShikiTheme());
    const observer = new MutationObserver(syncTheme);
    observer.observe(root, { attributes: true, attributeFilter: ["data-theme"] });
    syncTheme();
    return () => observer.disconnect();
  }, []);

  return theme;
}

/**
 * AI SDK Elements-style CodeBlock primitive, self-contained and Weft-skinned.
 * Highlights fenced code with shiki and preserves the surrounding markdown
 * link/file-ref semantics. While the fence is still open mid-stream (`loading`)
 * it stays on the plain fallback — no per-delta highlight work, and no
 * transient stream states in `htmlCache`.
 */
function CodeBlock({
  code,
  language,
  loading,
  className,
}: {
  code: string;
  language: string;
  loading?: boolean;
  className?: string;
}) {
  const theme = useShikiTheme();
  const cacheKey = `${theme}:${language}:${code}`;
  const [html, setHtml] = useState<string>(() => htmlCache.get(cacheKey) ?? "");

  useEffect(() => {
    if (loading) return;
    const cached = htmlCache.get(cacheKey);
    if (cached != null) {
      setHtml(cached);
      return;
    }
    setHtml("");
    let cancelled = false;
    const run = async () => {
      try {
        const out = await codeToHtml(code, { lang: language, theme });
        if (cancelled) return;
        htmlCache.set(cacheKey, out);
        setHtml(out);
      } catch {
        // Unknown language: fall back to plain-text highlighting.
        const out = await codeToHtml(code, { lang: "text", theme });
        if (cancelled) return;
        htmlCache.set(cacheKey, out);
        setHtml(out);
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [cacheKey, code, language, theme, loading]);

  if (html && !loading) {
    return (
      <div
        className={cn(
          "[&_code]:font-mono [&_pre]:overflow-x-auto [&_pre]:rounded-[var(--radius-md)] [&_pre]:border [&_pre]:border-border [&_pre]:p-3 [&_pre]:text-[11.5px]",
          className,
        )}
        dangerouslySetInnerHTML={{ __html: html }}
      />
    );
  }

  return (
    <pre
      className={cn(
        "overflow-x-auto rounded-[var(--radius-md)] border border-border bg-raised p-3 text-[11.5px]",
        className,
      )}
    >
      <code className="font-mono">{code}</code>
    </pre>
  );
}

/** Render a parsed node's children through markstream's own dispatcher. */
function renderNodeChildren(
  children: readonly unknown[] | undefined,
  props: NodeComponentProps<unknown>,
  suffix: string,
): ReactNode {
  const { ctx, renderNode, indexKey } = props;
  if (!children || children.length === 0 || !ctx || !renderNode) return null;
  return children.map((child, i) =>
    renderNode(child as ParsedNode, `${String(indexKey ?? "n")}-${suffix}${i}`, ctx),
  );
}

/** Markdown links: local paths become file refs; anything else opens via the OS
 *  opener (never in-webview navigation). */
function WeftLink(props: NodeComponentProps<LinkNode>) {
  const cwd = useContext(MarkdownCwdContext);
  const { node } = props;
  const children = renderNodeChildren(node.children, props, "l") ?? node.text;
  const href = String(node.href ?? "");
  // Raw-HTML anchors (`<a href="javascript:…">`) become link nodes WITHOUT
  // passing md.validateLink (that guards markdown link syntax only), so the
  // script-y denylist is enforced here too before anything reaches the DOM.
  if (!href || UNSAFE_HREF.test(href)) return <>{children}</>;
  // In-page anchor: let the DOM handle the jump, never the OS opener.
  if (href.startsWith("#")) return <a href={href}>{children}</a>;
  // linkify's fuzzy mode turns bare code-file names whose extension doubles as
  // a country TLD (`server.py`, `lib.rs`) into http:// links. A schemaless
  // autolink whose label reads as a single path takes the file-ref route the
  // label would have taken as plain prose; real domains (`example.com`,
  // `weft.dev`) are not path-like and stay web links.
  const label = String(node.text ?? "");
  if (label && href === `http://${label}`) {
    const segs = splitTextForPaths(label);
    if (segs.length === 1 && segs[0].type === "path") {
      return (
        <FilePathRef token={segs[0].value} cwd={cwd}>
          {segs[0].label ?? label}
        </FilePathRef>
      );
    }
  }
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
}

/** Inline code: a lone path-shaped token becomes a file ref, everything else
 *  stays a quiet code chip. */
function WeftInlineCode({ node }: NodeComponentProps<InlineCodeNode>) {
  const cwd = useContext(MarkdownCwdContext);
  const content = String(node.code ?? "");
  const pathSegs = splitTextForPaths(content);
  const singlePath = pathSegs.length === 1 && pathSegs[0].type === "path"
    ? pathSegs[0]
    : undefined;
  if (singlePath) {
    return (
      <FilePathRef
        token={singlePath.value}
        cwd={cwd}
        code
        isUrl={/^file:/i.test(singlePath.value)}
      >
        {singlePath.label ?? content}
      </FilePathRef>
    );
  }
  if (!hasLineLabelSyntax(content) && isPathLike(content, true)) {
    return (
      <FilePathRef token={content} cwd={cwd} code isUrl={/^file:/i.test(content)}>
        {content}
      </FilePathRef>
    );
  }
  return <code className={CODE_CHIP}>{content}</code>;
}

/** Prose text: path-shaped tokens become quiet file refs (the replacement for
 *  the old `filePathsRehype` HAST pass). Inside a link/file-ref the nested refs
 *  render inert — `FilePathRef` handles that via `InsideRefContext`. */
function WeftText({ node }: NodeComponentProps<TextNode>) {
  const cwd = useContext(MarkdownCwdContext);
  const content = String(node.content ?? "");
  const segs = splitTextForPaths(content);
  if (!segs.some((s) => s.type === "path")) return <>{content}</>;
  return (
    <>
      {segs.map((s, i) =>
        s.type === "path" ? (
          <FilePathRef key={i} token={s.value} cwd={cwd} isUrl={/^file:/i.test(s.value)}>
            {s.label ?? s.value}
          </FilePathRef>
        ) : (
          <Fragment key={i}>{s.value}</Fragment>
        ),
      )}
    </>
  );
}

/** Fenced code: shell languages get the copyable ShellSnippet, the rest go
 *  through the shiki CodeBlock. Also registered for the special languages
 *  (mermaid/d2/infographic) so they render as plain highlighted code instead of
 *  markstream's diagram components — Weft ships none of those optional peers. */
function WeftCodeBlock({ node }: NodeComponentProps<CodeBlockNode>) {
  const { t } = useTranslation();
  // A ```diff fence parses into split original/updated sides — node.code holds
  // only the updated text and node.language comes back empty. The transcript
  // wants the literal patch, so diff blocks render their fence body (raw,
  // ± markers intact) under shiki's diff grammar.
  const language = node.diff ? "diff" : parseLanguage(node.language);
  const code = node.diff ? String(node.raw ?? "") : String(node.code ?? "");
  if (isShellLanguage(language)) {
    return (
      <ShellSnippet
        code={code}
        label={t("ai.shellSnippet")}
        copyLabel={t("lead.copyMessage")}
        copiedLabel={t("lead.copied")}
      />
    );
  }
  return <CodeBlock code={code} language={language} loading={node.loading} className="my-2" />;
}

/** Raw inline HTML: keep the text content, drop the markup — no live elements
 *  ever mount from raw HTML (a raw `<a href>` must never navigate the webview).
 *  A childless node (`<img …>`, void/self-closing snippets) has no text to
 *  keep, so its source renders escaped instead of vanishing. */
function WeftHtmlInline(props: NodeComponentProps<HtmlInlineNode>) {
  const children = renderNodeChildren(props.node.children, props, "h");
  if (children) return <>{children}</>;
  return <>{String(props.node.content ?? props.node.raw ?? "")}</>;
}

/** Raw HTML blocks render as escaped source text — the words stay visible in
 *  the transcript, but nothing ever mounts as live DOM (a `<script>` block is
 *  just text here). Common harmless tags inside paragraphs — <div>, <br>,
 *  <b>, … — never reach this: the parser folds them into regular paragraph/
 *  hardbreak/strong nodes first. */
function WeftHtmlBlock({ node }: NodeComponentProps<HtmlBlockNode>) {
  const source = String(node.content ?? node.raw ?? "");
  if (!source.trim()) return null;
  return <div className="whitespace-pre-wrap">{source}</div>;
}

/** The blinking caret, injected into the parsed tree while streaming (see
 *  `appendCaret`). markstream's own typewriter cursor only runs in `content`
 *  mode — in `nodes` mode its effect bails out — and it also auto-hides between
 *  delta stalls, so Weft keeps its always-on caret instead. */
function WeftCaret() {
  return <span className={STREAM_CARET_CLASS} data-stream-caret aria-hidden />;
}

// Registered once at module scope; every <Markdown> instance opts in via
// customId="weft". Keys are parsed-node types, plus language names for the
// code-block special cases.
setCustomComponents("weft", {
  text: WeftText,
  link: WeftLink,
  inline_code: WeftInlineCode,
  code_block: WeftCodeBlock,
  mermaid: WeftCodeBlock,
  d2: WeftCodeBlock,
  infographic: WeftCodeBlock,
  html_inline: WeftHtmlInline,
  html_block: WeftHtmlBlock,
  [WEFT_CARET_TYPE]: WeftCaret,
} as unknown as CustomComponentMap);

/**
 * Renders agent output as markdown — headings, lists, code, tables, links —
 * scoped + sized to fit the transcript. Streaming-aware via markstream-react:
 * unfinished constructs (open fences, half-bold) render as their eventual
 * element instead of flashing raw markers, and the caret is the renderer's
 * typewriter cursor. Web links open in the OS browser via the opener; local
 * file paths (in links, inline code, or prose) become quiet file references —
 * ⌘-click to open, right-click to reveal — resolved against the session's
 * working dir (`cwd`).
 */
/** Streaming caret styling — shared with ChatTimeline's pre-text fallback so the
 *  markstream cursor (skinned in index.css) and the empty-state caret stay
 *  visually identical. */
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
  // Parser instance per mounted message: getMarkdown() has no registry (each
  // call constructs a fresh markdown-it), so this lives and dies with the
  // component; the id only namespaces generated DOM ids.
  const instanceId = useId();
  const md = useMemo(() => createWeftMarkdown(`weft-${instanceId}`), [instanceId]);
  const theme = useShikiTheme();
  const final = !caret;
  // Nodes mode (parse outside the renderer): direct control over ParseOptions,
  // and markdown-it-ts's stream cache keeps non-final re-parses incremental.
  const nodes = useMemo(() => {
    const parsed = parseMarkdownToStructure(text, md, { final, validateLink: allowHref });
    if (caret) injectCaret(parsed);
    return parsed;
  }, [text, md, final, caret]);
  return (
    <MarkdownCwdContext.Provider value={cwd}>
      <div className="weft-md text-[12.5px] leading-relaxed text-ink">
        <MarkdownRender
          nodes={nodes}
          final={final}
          customId="weft"
          typewriter={Boolean(caret)}
          fade={false}
          isDark={theme === "github-dark"}
          showTooltips={false}
        />
      </div>
    </MarkdownCwdContext.Provider>
  );
});
