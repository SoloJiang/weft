import { api } from "./api";
import { toast } from "../components/Toast";
import i18n from "../i18n";

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

/**
 * Normalize a token to a usable filesystem path (for copy + detection),
 * mirroring the backend resolver. For `isUrl` tokens (link hrefs) it strips the
 * `file://` scheme + optional `localhost` authority, drops a URL fragment/query,
 * percent-decodes, and normalizes the drive form; literal tokens (inline/prose)
 * keep `#`/`%` verbatim. Always splits off a trailing `:line[:col]` suffix.
 */
export function parsePathToken(
  token: string,
  isUrl = false,
): { path: string; line?: number; col?: number } {
  let t = token.trim();
  if (isUrl) {
    // URL token: strip scheme + localhost authority + fragment, percent-decode,
    // normalize the drive form. Literal tokens keep '#'/'%' as filename chars.
    const scheme = t.match(/^file:(?:\/\/)?/i); // file://, file:///, or minimal file:/
    if (scheme) t = t.slice(scheme[0].length).replace(/^localhost(?=\/)/i, "");
    const frag = t.search(/[#?]/);
    if (frag !== -1) t = t.slice(0, frag);
    try {
      t = decodeURIComponent(t);
    } catch {
      /* malformed %-escape — leave as-is */
    }
    t = t.replace(/^\/([A-Za-z]:[\\/])/, "$1"); // /C:/repo → C:/repo (mirror backend)
  }
  const m = t.match(/^(.*?):(\d+)(?::(\d+))?$/);
  // `m[1].length > 1` keeps a lone Windows drive letter (e.g. "C:5") intact.
  if (m && m[1].length > 1) {
    return { path: m[1], line: Number(m[2]), col: m[3] ? Number(m[3]) : undefined };
  }
  return { path: t };
}

// Extensions and bare manifest filenames that mark a token as a real file.
const CODE_EXT =
  "tsx?|jsx?|mjs|cjs|json|jsonc|rs|toml|lock|md|mdx|css|scss|less|html?|py|go|rb|php|java|kt|kts|swift|c|cc|cpp|cxx|h|hpp|m|mm|sh|bash|zsh|fish|ya?ml|sql|svg|vue|svelte|astro|txt|env|ini|cfg|conf|xml|gradle|proto|graphql|prisma";
const MANIFEST =
  "package(?:-lock)?\\.json|pnpm-lock\\.yaml|yarn\\.lock|tsconfig(?:\\.[\\w.-]+)?\\.json|Cargo\\.(?:toml|lock)|Dockerfile|Makefile|\\.gitignore|\\.env(?:\\.[\\w.-]+)?";
const EXT_RE = new RegExp(`\\.(?:${CODE_EXT})$`, "i");
const MANIFEST_RE = new RegExp(`(?:^|[/\\\\])(?:${MANIFEST})$`);

/**
 * Conservative test: is this token a local file path worth wiring up?
 * `allowSpaces` is for inline-code tokens, where the whole span is the path
 * (e.g. `My Dir/App.tsx`); prose keeps the no-space rule so words aren't joined.
 */
export function isPathLike(token: string, allowSpaces = false): boolean {
  const { path } = parsePathToken(token);
  if (!path) return false;
  if (!allowSpaces && /\s/.test(path)) return false;
  if (/^(\/|\\|~[\\/]?|\.\.?[\\/])/.test(path)) return true; // /abs \abs ~ ~/ ~\ ./ .\ ../ ..\
  if (WIN_DRIVE.test(path)) return true;
  if (EXT_RE.test(path)) return true; // foo.ts, a/b/foo.ts, a\b\foo.ts (known extension)
  if (MANIFEST_RE.test(path)) return true; // Dockerfile, Makefile, .gitignore
  return false;
}

// ---- bare-prose detection ---------------------------------------------------

export type Seg = { type: "text" | "path"; value: string };

// CJK full-width brackets / quotes / stops common in Chinese prose. Chinese has
// no inter-word spaces, so these are treated as token DELIMITERS (split on them),
// not just peeled — otherwise `lib/x.rs，然后` stays one token and never matches.
const CJK_PUNCT = "（）【】「」『』〔〕〈〉《》〖〗。，、；：！？…—～·｜“”‘’";
// CJK ideographs (kana, CJK Ext-A, CJK Unified, compat, Hangul). Chinese has no
// inter-word spaces, so an ideograph run is a hard boundary too — this isolates
// an ASCII path embedded with no delimiter, e.g. `修改src/App.tsx文件`. (An
// all-CJK-named path won't be detected, but it's left as text, not mangled.)
const CJK_IDEO = "\\u3040-\\u30ff\\u3400-\\u4dbf\\u4e00-\\u9fff\\uf900-\\ufaff\\uac00-\\ud7af";
const LIST_SEP = ",;"; // ASCII list separators between adjacent path refs (a.ts,b.ts)
const DELIM = new RegExp(`(\\s+|[${LIST_SEP}${CJK_IDEO}${CJK_PUNCT}]+)`);
const IS_DELIM = new RegExp(`^(?:\\s|[${LIST_SEP}${CJK_IDEO}${CJK_PUNCT}])`);
// ASCII punctuation that hugs a path is peeled back into the text run.
const LEAD_PUNCT = /^[([{<'"`]+/;
const TAIL_PUNCT = /[)\]}>'"`.,;:!?]+$/;

// Cheap gate before the heavier isPathLike: a separator, a dot (extensions or
// dotfiles), or a dotless manifest name (optionally with a `:line` suffix).
// Lets ordinary prose words bail out without a regex match.
function couldBePath(core: string): boolean {
  if (/[/\\.]/.test(core)) return true;
  const head = core.replace(/:\d+(?::\d+)?$/, ""); // drop :line for the dotless check
  return head === "Dockerfile" || head === "Makefile";
}

function pushText(segs: Seg[], v: string): void {
  const last = segs[segs.length - 1];
  if (last && last.type === "text") last.value += v;
  else segs.push({ type: "text", value: v });
}

/**
 * Split prose into text/path segments (re-joining to the original string). Splits
 * on whitespace AND CJK punctuation so bare names (`Cargo.toml`) and separator
 * paths (`src/foo.ts`, `src\foo.ts`) are caught even when hugged by Chinese
 * punctuation (`（src/App.tsx）`, `lib/x.rs，然后`); ASCII punctuation is peeled.
 */
export function splitTextForPaths(text: string): Seg[] {
  const segs: Seg[] = [];
  for (const part of text.split(DELIM)) {
    if (!part) continue;
    if (IS_DELIM.test(part)) {
      pushText(segs, part); // whitespace / CJK-punctuation run → text
      continue;
    }
    const lead = part.match(LEAD_PUNCT)?.[0] ?? "";
    let core = part.slice(lead.length);
    const tail = core.match(TAIL_PUNCT)?.[0] ?? "";
    if (tail) core = core.slice(0, core.length - tail.length);
    if (core && couldBePath(core) && isPathLike(core)) {
      if (lead) pushText(segs, lead);
      segs.push({ type: "path", value: core });
      if (tail) pushText(segs, tail);
    } else {
      pushText(segs, part);
    }
  }
  return segs;
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
            children: [{ type: "text", value: s.value }],
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
