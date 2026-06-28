import { api } from "./api";
import { isPathLike, parsePathToken, splitTextForPaths } from "./filePathParsing";
import { toast } from "../components/Toast";
import i18n from "../i18n";

export { isPathLike, parsePathToken, splitTextForPaths } from "./filePathParsing";

/**
 * Turning the file paths agents mention in chat into real, openable references.
 *
 * Pipeline: classify a markdown link's href (web URL vs local path); detect
 * path-shaped tokens in inline code and plain prose; resolve + open them via the
 * backend (cross-platform, against the session's working dir). Detection only
 * *styles* — existence is verified on click, so the regex can stay conservative
 * without being perfect (a wrong guess just shows a quiet "not found" toast).
 */

// ---- classification & parsing ----------------------------------------------

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
        if (s.type === "text") out.push({ type: "text", value: s.value });
        else
          out.push({
            type: "element",
            tagName: "span",
            properties: { dataFilepath: s.value },
            children: [{ type: "text", value: s.label ?? s.value }],
          });
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

// ---- actions ----------------------------------------------------------------

function failToast(e: unknown): void {
  const code = String(e);
  toast(i18n.t(code.includes("not_found") ? "fileLink.notFound" : "fileLink.openFailed"));
}

// `isUrl` marks a token that came from a markdown link href (URI semantics);
// false means a literal path from inline code or prose ('#'/'%' are verbatim).

/** Open a file reference with the OS default app. */
export async function openFileRef(token: string, cwd?: string, isUrl = false): Promise<void> {
  try {
    await api.openPath(token, cwd, isUrl);
  } catch (e) {
    failToast(e);
  }
}

/** Reveal a file reference's containing folder (selecting the item). */
export async function revealFileRef(token: string, cwd?: string, isUrl = false): Promise<void> {
  try {
    await api.revealPathIn(token, cwd, isUrl);
  } catch (e) {
    failToast(e);
  }
}

/** Copy a file reference's bare path (scheme/fragment/line suffix stripped for URLs). */
export function copyFilePath(token: string, isUrl = false): void {
  void navigator.clipboard?.writeText(parsePathToken(token, isUrl).path);
  toast(i18n.t("resume.copied"));
}

// ---- right-click menu store (mirrors components/Toast.tsx) ------------------

export type FileMenuState = {
  x: number;
  y: number;
  token: string;
  cwd?: string;
  isUrl?: boolean;
};

let menuState: FileMenuState | null = null;
const menuListeners = new Set<() => void>();

function notifyMenu() {
  for (const l of menuListeners) l();
}

export function openFileMenu(s: FileMenuState) {
  menuState = s;
  notifyMenu();
}

export function closeFileMenu() {
  if (menuState) {
    menuState = null;
    notifyMenu();
  }
}

export function subscribeFileMenu(cb: () => void) {
  menuListeners.add(cb);
  return () => {
    menuListeners.delete(cb);
  };
}

export function fileMenuSnapshot(): FileMenuState | null {
  return menuState;
}
