import { getMarkdown, type MarkdownIt } from "stream-markdown-parser";

/** Script-y schemes are never handed to the DOM href or the OS opener. */
export const UNSAFE_HREF = /^\s*(?:javascript|data|vbscript):/i;

/** Weft's link policy, applied at the markdown-it layer. Anything an agent may
 *  legitimately mention stays a link (file:, vscode://, ms-settings:, relative
 *  paths, …); only script-y schemes are rejected. This also REPLACES
 *  markdown-it-ts's stock `validateLink`, which mis-drops links (verified: a
 *  `` `code` `` span + `[x](file:///…)` + a later link in one paragraph eats the
 *  file link) — keep this override even if the policy ever becomes allow-all. */
export function allowHref(url: string): boolean {
  return !UNSAFE_HREF.test(url);
}

function configureWeftMarkdownIt(md: MarkdownIt): void {
  (md as { validateLink?: (url: string) => boolean }).validateLink = allowHref;
  // No TeX in Weft chat: agents write dollar amounts far more often than math
  // (`$100 and $200` would otherwise parse as an inline formula).
  md.disable(["math", "math_block", "explicit_math_block"], true);
  // Chat text gets copied back out verbatim (commands, config prose), so no
  // typographic rewriting: `"model"` must not become curly-quoted, `--` must
  // not become a dash, `(c)` must not become ©.
  (md as unknown as { set: (opts: { typographer: boolean }) => void }).set({
    typographer: false,
  });
}

/** A Weft-configured markdown-it instance. One per mounted message — the
 *  factory has no registry, so the instance lives and dies with its owner and
 *  the id only namespaces generated DOM ids. */
export function createWeftMarkdown(id: string): MarkdownIt {
  return getMarkdown(id, { apply: [configureWeftMarkdownIt] });
}
