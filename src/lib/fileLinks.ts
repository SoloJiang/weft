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

const SCHEME_SLASH = /^[a-z][a-z0-9+.-]*:\/\//i; // http://, https://, codex://, vscode://
const MAILTO_TEL = /^(mailto|tel):/i;
const WIN_DRIVE = /^[a-zA-Z]:[\\/]/;

/**
 * Classify a markdown link's href. Explicit links are lenient: anything that
 * isn't clearly a web/app URL or in-page anchor is treated as a local file (the
 * agent deliberately linked it).
 */
export function classifyHref(href: string): HrefKind {
  const h = href.trim();
  if (/^file:\/\//i.test(h)) return { kind: "file", token: h };
  if (h.startsWith("#")) return { kind: "web", url: h }; // in-page anchor — leave alone
  if (SCHEME_SLASH.test(h) || MAILTO_TEL.test(h)) return { kind: "web", url: h };
  return { kind: "file", token: h };
}

/** Strip a `file://` scheme and a trailing `:line[:col]` editor suffix. */
export function parsePathToken(token: string): { path: string; line?: number; col?: number } {
  let t = token.trim();
  if (/^file:\/\//i.test(t)) {
    t = t.replace(/^file:\/\//i, "");
    try {
      t = decodeURI(t);
    } catch {
      /* leave encoded */
    }
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
const MANIFEST_RE = new RegExp(`(?:^|/)(?:${MANIFEST})$`);

/** Conservative test: is this token a local file path worth wiring up? */
export function isPathLike(token: string): boolean {
  const { path } = parsePathToken(token);
  if (!path || /\s/.test(path)) return false;
  if (/^(\/|~\/?|\.\.?\/)/.test(path)) return true; // /abs, ~, ~/, ./, ../
  if (WIN_DRIVE.test(path)) return true;
  if (path.includes("/") && EXT_RE.test(path)) return true; // a/b/foo.ts
  if (MANIFEST_RE.test(path)) return true; // Cargo.toml, package.json, Makefile
  return false;
}

// ---- bare-prose detection ---------------------------------------------------

export type Seg = { type: "text" | "path"; value: string };

// A whitespace-delimited chunk containing a path separator; surrounding
// punctuation is peeled back into text and the core re-validated.
const SEP_TOKEN = /\S*[/\\]\S*/g;
const LEAD_PUNCT = /^[([{<'"`]+/;
const TAIL_PUNCT = /[)\]}>'"`.,;:!?]+$/;

/** Split prose into text/path segments (re-joining to the original string). */
export function splitTextForPaths(text: string): Seg[] {
  const segs: Seg[] = [];
  let last = 0;
  for (const match of text.matchAll(SEP_TOKEN)) {
    const idx = match.index ?? 0;
    const chunk = match[0];
    const lead = chunk.match(LEAD_PUNCT)?.[0] ?? "";
    let core = chunk.slice(lead.length);
    const tail = core.match(TAIL_PUNCT)?.[0] ?? "";
    if (tail) core = core.slice(0, core.length - tail.length);
    if (!core || !isPathLike(core)) continue; // leave in the surrounding text run
    const before = text.slice(last, idx) + lead;
    if (before) segs.push({ type: "text", value: before });
    segs.push({ type: "path", value: core });
    last = idx + lead.length + core.length; // tail stays as text for the next gap
  }
  const rest = text.slice(last);
  if (rest) segs.push({ type: "text", value: rest });
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
    if (child.type === "text" && typeof child.value === "string" && child.value.includes("/")) {
      const segs = splitTextForPaths(child.value);
      if (segs.length <= 1) {
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

/** Open a file reference with the OS default app. */
export async function openFileRef(token: string, cwd?: string): Promise<void> {
  try {
    await api.openPath(token, cwd);
  } catch (e) {
    failToast(e);
  }
}

/** Reveal a file reference's containing folder (selecting the item). */
export async function revealFileRef(token: string, cwd?: string): Promise<void> {
  try {
    await api.revealPath(token, cwd);
  } catch (e) {
    failToast(e);
  }
}

/** Copy a file reference's bare path (scheme + line suffix stripped). */
export function copyFilePath(token: string): void {
  void navigator.clipboard?.writeText(parsePathToken(token).path);
  toast(i18n.t("resume.copied"));
}

// ---- right-click menu store (mirrors components/Toast.tsx) ------------------

export type FileMenuState = { x: number; y: number; token: string; cwd?: string };

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
