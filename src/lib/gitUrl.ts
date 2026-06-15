/**
 * Recognize git repository URLs pasted in arbitrary text and derive repo names.
 *
 * Shared by the Add-repo dialog (batch paste) and the clone slash-command. Pure +
 * dependency-free. Two accepted URL shapes:
 *   - scheme URL:  https:// | http:// | ssh:// | git:// | file://  + host/path
 *   - scp-style:   user@host:org/repo(.git)   (classic `git@github.com:a/b.git`)
 */

// `,`/`;`/whitespace/wrappers terminate a URL body so separated pastes split.
// `]` is allowed so bracketed IPv6 hosts (`[::1]`, `[2001:db8::1]`) survive; a
// trailing `]` wrapper is peeled by trimUrl below.
const SCHEME_URL = /\b(?:https?|ssh|git|file):\/\/[^\s<>"'`),;]+/gi;
const SCP_URL = /\b[A-Za-z0-9._-]+@[A-Za-z0-9._-]+:[^\s<>"'`),;]+/gi;
// scp-style WITHOUT a user (`host.tld:path`, the `[<user>@]<host>:<path>` form).
// Require a dotted host AND a `/` in the path so it doesn't swallow tokens like
// `file.ts:42`.
const SCP_NOUSER = /\b[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+:[^\s<>"'`),;]*\/[^\s<>"'`),;]+/gi;

/** Strip wrapping/trailing punctuation a paste often carries around a URL. */
function trimUrl(raw: string): string {
  return raw
    .trim()
    .replace(/^[-*>\s([{<'"`]+/, "")
    .replace(/[)\]}>'"`.,;]+$/, "");
}

/**
 * Normalized key for dedup — drop trailing `.git`/slashes; lowercase the HOST
 * only (case-insensitive), keep the repo path's case (case-sensitive git hosts:
 * `Team/App` and `team/App` are distinct repos).
 */
function dedupKey(url: string): string {
  // Trim trailing slashes BEFORE `.git` so `repo.git/` and `repo` key the same.
  const base = url.replace(/\/+$/, "").replace(/\.git$/i, "");
  const scheme = base.match(/^([a-z][a-z0-9+.-]*:\/\/[^/]*)(\/.*)?$/i);
  if (scheme) return scheme[1].toLowerCase() + (scheme[2] ?? "");
  const scp = base.match(/^([^:]+):(.*)$/); // [user@]host : path
  if (scp) {
    const authority = scp[1];
    const at = authority.lastIndexOf("@");
    // Host is case-insensitive; the optional SSH user is NOT (Unix usernames).
    const user = at >= 0 ? authority.slice(0, at + 1) : "";
    const host = at >= 0 ? authority.slice(at + 1) : authority;
    return `${user}${host.toLowerCase()}:${scp[2]}`;
  }
  return base.toLowerCase();
}

/**
 * Extract recognized git URLs from arbitrary pasted text, in first-seen (paste)
 * order, deduped. SCP matches that fall inside a scheme URL (e.g. the
 * `user@host:443/…` slice of `https://user@host:443/…`) are skipped — a scheme
 * URL always starts before its inner scp slice, so claim-on-lower-start drops it.
 */
export function parseGitUrls(text: string): string[] {
  const matches: Array<{ raw: string; start: number; end: number }> = [];
  for (const m of text.matchAll(SCHEME_URL)) {
    matches.push({ raw: m[0], start: m.index ?? 0, end: (m.index ?? 0) + m[0].length });
  }
  for (const m of text.matchAll(SCP_URL)) {
    matches.push({ raw: m[0], start: m.index ?? 0, end: (m.index ?? 0) + m[0].length });
  }
  for (const m of text.matchAll(SCP_NOUSER)) {
    matches.push({ raw: m[0], start: m.index ?? 0, end: (m.index ?? 0) + m[0].length });
  }
  matches.sort((a, b) => a.start - b.start);

  const out: string[] = [];
  const seen = new Set<string>();
  const claimed: Array<[number, number]> = [];
  for (const { raw, start, end } of matches) {
    if (claimed.some(([s, e]) => start < e && end > s)) continue; // inside a claimed URL
    const url = trimUrl(raw);
    if (!url) continue;
    claimed.push([start, end]);
    const key = dedupKey(url);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(url);
  }
  return out;
}

/** Derive a repo folder name from a git URL (basename, sans `.git`). */
export function repoNameFromUrl(url: string): string {
  return (
    url
      .trim()
      .replace(/[\\/]+$/, "") // trim trailing slashes BEFORE `.git` so `repo.git/` → `repo`
      .replace(/\.git$/i, "")
      .split(/[\\/:]/)
      .filter(Boolean)
      .pop() ?? ""
  );
}
