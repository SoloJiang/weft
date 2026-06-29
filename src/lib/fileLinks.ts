import { api } from "./api";
import { parsePathToken } from "./filePathParsing";
import { toast } from "../components/Toast";
import i18n from "../i18n";

export { isPathLike, parsePathToken, displayPath, splitTextForPaths } from "./filePathParsing";
export { classifyHref, filePathsRehype, type HrefKind } from "./fileLinkMarkdown";

/**
 * Turning the file paths agents mention in chat into real, openable references.
 *
 * Pipeline: classify a markdown link's href (web URL vs local path); detect
 * path-shaped tokens in inline code and plain prose; resolve + open them via the
 * backend (cross-platform, against the session's working dir). Detection only
 * *styles* — existence is verified on click, so the regex can stay conservative
 * without being perfect (a wrong guess just shows a quiet "not found" toast).
 */

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
