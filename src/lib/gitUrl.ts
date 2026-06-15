/**
 * Recognize git repository URLs pasted in arbitrary text and derive repo names.
 *
 * Shared by the Add-repo dialog (batch paste) and the clone slash-command. Pure +
 * dependency-free. Two accepted URL shapes:
 *   - scheme URL:  https:// | http:// | ssh:// | git://  + host/path
 *   - scp-style:   user@host:org/repo(.git)   (classic `git@github.com:a/b.git`)
 */

// `,` and `;` are excluded from the body so comma/semicolon-separated pastes
// split even without surrounding whitespace (`a.git,https://…/b.git`).
const SCHEME_URL = /\b(?:https?|ssh|git):\/\/[^\s<>"'`),;\]]+/gi;
const SCP_URL = /\b[A-Za-z0-9._-]+@[A-Za-z0-9._-]+:[^\s<>"'`),;\]]+/gi;

/** Strip wrapping/trailing punctuation a paste often carries around a URL. */
function trimUrl(raw: string): string {
  return raw
    .trim()
    .replace(/^[-*>\s([{<'"`]+/, "")
    .replace(/[)\]}>'"`.,;]+$/, "");
}

/** Normalized key for dedup — drop trailing `.git`/slashes, lowercase. */
function dedupKey(url: string): string {
  return url.toLowerCase().replace(/\.git$/, "").replace(/\/+$/, "");
}

/**
 * Extract recognized git URLs from arbitrary pasted text, in first-seen order,
 * deduped. SCP matches that fall inside an already-matched scheme URL (e.g. the
 * `user@host:443/…` slice of `https://user@host:443/…`) are skipped.
 */
export function parseGitUrls(text: string): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  const claimed: Array<[number, number]> = [];

  const consider = (raw: string, start: number, end: number) => {
    if (claimed.some(([s, e]) => start < e && end > s)) return; // overlaps a scheme URL
    const url = trimUrl(raw);
    if (!url) return;
    claimed.push([start, end]);
    const key = dedupKey(url);
    if (seen.has(key)) return;
    seen.add(key);
    out.push(url);
  };

  for (const m of text.matchAll(SCHEME_URL)) {
    consider(m[0], m.index ?? 0, (m.index ?? 0) + m[0].length);
  }
  for (const m of text.matchAll(SCP_URL)) {
    consider(m[0], m.index ?? 0, (m.index ?? 0) + m[0].length);
  }
  return out;
}

/** Derive a repo folder name from a git URL (basename, sans `.git`). */
export function repoNameFromUrl(url: string): string {
  return (
    url
      .trim()
      .replace(/\.git$/i, "")
      .replace(/[\\/]+$/, "")
      .split(/[\\/:]/)
      .filter(Boolean)
      .pop() ?? ""
  );
}
