/**
 * Recognize git repository URLs pasted in arbitrary text and derive repo names.
 *
 * Shared by the Add-repo dialog (batch paste) and the clone slash-command. Pure +
 * dependency-free. Two accepted URL shapes:
 *   - scheme URL:  https:// | http:// | ssh:// | git:// | file://  + host/path
 *   - scp-style:   user@host:org/repo(.git)   (classic `git@github.com:a/b.git`)
 */

// Only whitespace, `<`/`>`, quotes, `,` and `;` terminate a URL body (so
// separated pastes split). Brackets/parens are allowed in the body — bracketed
// IPv6 hosts (`[::1]`) and paths with `(…)` survive — and any trailing wrapper
// (`)`, `]`, `}`) is peeled by trimUrl below.
const SCHEME_URL = /\b(?:https?|ssh|git|file):\/\/[^\s<>"'`,;]+/gi;
const SCP_URL = /\b[A-Za-z0-9._-]+@[A-Za-z0-9._-]+:[^\s<>"'`,;]+/gi;
// scp-style WITHOUT a user (`host.tld:path`, the `[<user>@]<host>:<path>` form).
// Require a dotted host AND a `/` in the path so it doesn't swallow tokens like
// `file.ts:42`.
const SCP_NOUSER = /\b[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+:[^\s<>"'`,;]*\/[^\s<>"'`,;]+/gi;
// ANY `scheme://…` URL (modeled or not, e.g. ftp://) — used ONLY to exclude scp
// candidates that are really a scheme URL's authority. A credentialed authority
// like `ftp://user@host:21/a/b` spawns two scp candidates (`user@host:21/…` and
// the no-user `host:21/…`); both start inside this span and must be dropped, or
// the no-user matcher would feed the backend a scheme-/port-stripped scp slice.
const ANY_SCHEME_URL = /\b[a-z][a-z0-9+.-]*:\/\/[^\s<>"'`,;]+/gi;

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
export function gitUrlKey(url: string): string {
  // Trim trailing slashes BEFORE `.git` so `repo.git/` and `repo` key the same.
  const base = url.replace(/\/+$/, "").replace(/\.git$/i, "");
  // Host is case-insensitive; the optional userinfo / SSH user is NOT.
  const lowerHost = (authority: string) => {
    const at = authority.lastIndexOf("@");
    const user = at >= 0 ? authority.slice(0, at + 1) : "";
    const host = at >= 0 ? authority.slice(at + 1) : authority;
    return user + host.toLowerCase();
  };
  const scheme = base.match(/^([a-z][a-z0-9+.-]*:\/\/)([^/]*)(\/.*)?$/i);
  if (scheme) return scheme[1].toLowerCase() + lowerHost(scheme[2]) + (scheme[3] ?? "");
  const scp = base.match(/^([^:]+):(.*)$/); // [user@]host : path
  if (scp) return `${lowerHost(scp[1])}:${scp[2]}`;
  return base.toLowerCase();
}

/**
 * Extract recognized git URLs from arbitrary pasted text, in first-seen (paste)
 * order, deduped. An scp match that is actually the authority of a scheme URL —
 * modeled (`https://user@host:443/…`) or not (`ftp://user@host:21/…`) — is
 * dropped: any scp candidate starting inside a `scheme://…` span is skipped, so
 * an unmodeled-scheme URL recognizes as nothing and the caller's raw fallback
 * can hand git the whole URL instead of a scheme-stripped slice.
 */
export function parseGitUrls(text: string): string[] {
  const schemeSpans: Array<[number, number]> = [];
  for (const m of text.matchAll(ANY_SCHEME_URL)) {
    const s = m.index ?? 0;
    schemeSpans.push([s, s + m[0].length]);
  }
  const matches: Array<{ raw: string; start: number; end: number; scp: boolean }> = [];
  for (const m of text.matchAll(SCHEME_URL)) {
    matches.push({ raw: m[0], start: m.index ?? 0, end: (m.index ?? 0) + m[0].length, scp: false });
  }
  for (const m of text.matchAll(SCP_URL)) {
    matches.push({ raw: m[0], start: m.index ?? 0, end: (m.index ?? 0) + m[0].length, scp: true });
  }
  for (const m of text.matchAll(SCP_NOUSER)) {
    matches.push({ raw: m[0], start: m.index ?? 0, end: (m.index ?? 0) + m[0].length, scp: true });
  }
  matches.sort((a, b) => a.start - b.start);

  const out: string[] = [];
  const seen = new Set<string>();
  const claimed: Array<[number, number]> = [];
  for (const { raw, start, end, scp } of matches) {
    if (claimed.some(([s, e]) => start < e && end > s)) continue; // inside a claimed URL
    // Drop every scp candidate sitting inside a `scheme://…` authority (incl.
    // schemes we don't model, e.g. ftp://) so we never emit a scheme-stripped
    // slice; the caller's raw fallback hands the full URL to git instead.
    if (scp && schemeSpans.some(([s, e]) => start >= s && start < e)) continue;
    const url = trimUrl(raw);
    if (!url) continue;
    claimed.push([start, end]);
    const key = gitUrlKey(url);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(url);
  }
  return out;
}

// A token that can ONLY be a standalone source — never a fragment of a local
// path with spaces: a scheme URL (`scheme://…`), an scp/alias colon-path
// (`[user@]host:org/repo`), or anything the parser already recognizes. Its
// presence proves whitespace on its line separates sources.
function isHardSource(tok: string): boolean {
  return tok.includes("://") || /^[^\s:\\]+:[^\s\\]*\/[^\s]*$/.test(tok) || parseGitUrls(tok).length > 0;
}

// A token worth cloning (vs. a prose label like `Repos:` or a markdown bullet
// `-`/`*` to drop when a line is split): a hard source, a path (has `/` or `\`),
// or a bare `name.git`.
function looksLikeSource(tok: string): boolean {
  return isHardSource(tok) || /[/\\]/.test(tok) || /\.git$/i.test(tok);
}

/**
 * Parse a paste box of clone sources into a deduped list, in paste order.
 *
 * Newline always separates sources. Within a line, whitespace/commas separate
 * sources ONLY when the line carries a "hard source" token (a scheme `://…` or
 * scp/alias colon-path) that cannot be a fragment of a spaced path — then the
 * line is split, source-shaped tokens are kept, and prose (labels like `Repos:`,
 * bullets `-`/`*`) is dropped. A line without that evidence is taken as ONE
 * source, so a local path with spaces (`/Users/me/My Projects/repo`) survives.
 *
 * Each kept source is wrapper-trimmed (backticks, bullets, brackets, trailing
 * punctuation), then normalized to its recognized git URL, or kept verbatim
 * (local path, ssh alias `gh:org/repo`, `ftp://…`, a typo) so `git clone` can
 * attempt it and report per-row — never silently dropped.
 */
export function parseCloneSources(text: string): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  const add = (token: string) => {
    const urls = parseGitUrls(token);
    if (urls.length > 0) {
      for (const u of urls) {
        const key = gitUrlKey(u);
        if (seen.has(key)) continue;
        seen.add(key);
        out.push(u);
      }
      return;
    }
    const raw = trimUrl(token);
    if (raw === "") return;
    const key = `raw:${raw}`;
    if (seen.has(key)) return;
    seen.add(key);
    out.push(raw);
  };
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (line === "") continue;
    const tokens = line.split(/[\s,]+/).filter(Boolean);
    if (tokens.length > 1 && tokens.some(isHardSource)) {
      for (const tok of tokens) if (looksLikeSource(tok)) add(tok);
    } else {
      add(line);
    }
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
