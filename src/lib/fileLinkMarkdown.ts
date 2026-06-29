import { isPathLike, splitTextForPaths } from "./filePathParsing.ts";

export type HrefKind = { kind: "web"; url: string } | { kind: "file"; token: string };

const SCHEME = /^[a-z][a-z0-9+.-]*:/i; // http:, mailto:, ms-settings:, vscode-insiders:, …
const LINE_SUFFIX_HEAD = /^([^:]*):\d+(?::\d+)?$/; // path:line[:col] → captures head
const WIN_DRIVE = /^[a-zA-Z]:[\\/]/;

/**
 * Classify a markdown link's href. Any real URI scheme (http://, mailto:, tel:,
 * ms-settings:, codex://, vscode-insiders://, …) and protocol-relative URLs open
 * as web/app links via the opener; everything else — absolute/relative paths,
 * `path:line`, drive paths — is treated as a local file the agent linked.
 */
export function classifyHref(href: string): HrefKind {
  const h = href.trim();
  // Any `file:` URI (file://host/p, file:///p, or the minimal file:/p) is a file ref.
  if (/^file:/i.test(h)) return { kind: "file", token: h };
  if (h.startsWith("#")) return { kind: "web", url: h }; // in-page anchor
  // Protocol-relative URL — give the opener a concrete scheme (no page to inherit one from).
  if (h.startsWith("//")) return { kind: "web", url: `https:${h}` };
  if (SCHEME.test(h) && !WIN_DRIVE.test(h)) {
    // A scheme prefix is a URI UNLESS it's a `path:line` ref whose head is itself
    // path-like (Cargo.toml:42) — distinguishes files from scheme:opaque links
    // (tel:155…, ms-settings:…).
    const m = h.match(LINE_SUFFIX_HEAD);
    if (m && isPathLike(m[1])) return { kind: "file", token: h };
    return { kind: "web", url: h };
  }
  return { kind: "file", token: h };
}

interface HNode {
  type: string;
  tagName?: string;
  value?: string;
  properties?: Record<string, unknown>;
  children?: HNode[];
}

const SKIP_TAGS = new Set(["a", "code", "pre", "script", "style"]);

function walkHast(node: HNode): void {
  const kids = node.children;
  if (!kids || kids.length === 0) return;
  const out: HNode[] = [];
  for (const child of kids) {
    if (child.type === "text" && typeof child.value === "string") {
      const segs = splitTextForPaths(child.value);
      // Skip only when nothing matched — a node that is *exactly* one path
      // (e.g. a bullet containing just `src/App.tsx`) must still be wrapped.
      if (!segs.some((s) => s.type === "path")) {
        out.push(child);
        continue;
      }
      for (const s of segs) {
        if (s.type === "text") {
          out.push({ type: "text", value: s.value });
        } else {
          out.push({
            type: "element",
            tagName: "span",
            properties: { dataFilepath: s.value },
            children: [{ type: "text", value: s.label ?? s.value }],
          });
        }
      }
    } else if (child.type === "element") {
      if (!child.tagName || !SKIP_TAGS.has(child.tagName)) walkHast(child);
      out.push(child);
    } else {
      out.push(child);
    }
  }
  node.children = out;
}

/** rehype plugin: wrap path-shaped prose tokens in `<span data-filepath>`. */
export function filePathsRehype() {
  return (tree: unknown) => {
    walkHast(tree as HNode);
  };
}
