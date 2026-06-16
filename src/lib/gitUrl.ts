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
// Remote-helper source `<transport>::<address>` (git-clone docs). The address
// may be scp- or URL-like, so — like ANY_SCHEME_URL — it is used ONLY to exclude
// matches inside the ADDRESS (`git@host:org/repo` AND its no-user `host:org/repo`
// slice in `hg::git@host:org/repo`) so the whole helper source stays raw.
const HELPER_URL = /\b[a-z][a-z0-9+.-]*::[^\s<>"'`,;]+/gi;

/**
 * Strip wrapping/trailing punctuation a paste often carries around a source,
 * WITHOUT mangling brackets that are part of the source:
 *   - a `(`/`[`/`{`/`<` is peeled only as half of a pair wrapping the WHOLE token
 *     (`[https://…]` → `https://…`), so an IPv6 scp host whose `]` sits mid-token
 *     (`[::1]:repo.git`) keeps its leading `[`;
 *   - a trailing `)`/`]`/`}` is peeled only when UNBALANCED, so a path ending in
 *     a balanced bracket survives (`/tmp/src(foo)`, `file:///tmp/src(foo)`).
 */
function trimUrl(raw: string): string {
  const closerOf: Record<string, string> = { "(": ")", "[": "]", "{": "}", "<": ">" };
  let s = raw.trim();
  let prev = "";
  while (s !== prev) {
    prev = s;
    // leading paste noise: bullets, blockquote `>`, quotes, backticks, spaces
    s = s.replace(/^[-*>\s'"`]+/, "");
    // a bracket pair wrapping the ENTIRE token
    const first = s[0];
    if (first && closerOf[first] && s.endsWith(closerOf[first])) {
      s = s.slice(1, -1).trim();
      continue;
    }
    // trailing paste noise; an UNBALANCED closing bracket (a balanced one is path)
    const last = s[s.length - 1];
    if (last && "'\"`.,;>".includes(last)) {
      s = s.slice(0, -1);
    } else if (last === ")" || last === "]" || last === "}") {
      const open = last === ")" ? "(" : last === "]" ? "[" : "{";
      let balance = 0;
      for (const c of s) balance += c === open ? 1 : c === last ? -1 : 0;
      if (balance < 0) s = s.slice(0, -1);
    }
  }
  return s;
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
 * order, deduped. A match that is really part of a LARGER single source is
 * dropped so the caller's raw fallback hands git the whole source untouched:
 *   - an scp candidate inside a `scheme://…` authority — modeled
 *     (`https://user@host:443/…`) or not (`ftp://user@host:21/…`);
 *   - a modeled scheme nested in a custom transport (`ssh://` inside
 *     `git+ssh://host/repo` — the inner match starts past the span's start);
 *   - any match inside a remote-helper address (`https://…` or every scp slice
 *     of `git@host:org/repo` inside `hg::…`).
 */
export function parseGitUrls(text: string): string[] {
  const schemeSpans: Array<[number, number]> = [];
  for (const m of text.matchAll(ANY_SCHEME_URL)) {
    const s = m.index ?? 0;
    schemeSpans.push([s, s + m[0].length]);
  }
  // Exclusion spans covering the ADDRESS of each remote-helper source (after the
  // `::`), so EVERY match inside it is dropped — not just the slice right after.
  const helperSpans: Array<[number, number]> = [];
  for (const m of text.matchAll(HELPER_URL)) {
    const s = m.index ?? 0;
    helperSpans.push([s + m[0].indexOf("::") + 2, s + m[0].length]);
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
    // Inside a `scheme://…` span: an scp candidate (start ≥ span start) is that
    // scheme's authority; a nested scheme match (start AFTER the span start) is
    // the inner URL of a custom transport like git+ssh://. Drop either.
    if (schemeSpans.some(([s, e]) => (scp ? start >= s : s < start) && start < e)) continue;
    // Any match inside a remote-helper address (hg::git@host:org/repo) — keep the
    // whole helper source raw rather than emitting its inner URL / scp slice.
    if (helperSpans.some(([s, e]) => start >= s && start < e)) continue;
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
// path with spaces: a scheme URL (`scheme://…`), an scp/alias colon-path whose
// path has a `/` (`[user@]host:org/repo`) or ends in `.git` (`host:repo.git`),
// or anything the parser already recognizes. Its presence proves whitespace on
// its line separates sources. (A bare `label:` or `file.ts:42` does NOT match.)
function isHardSource(tok: string): boolean {
  return (
    tok.includes("://") ||
    /^[^\s:\\]+:(?:[^\s\\]*\/[^\s]*|[^\s\\]*\.git)$/i.test(tok) ||
    parseGitUrls(tok).length > 0
  );
}

// A real filesystem path — absolute, `./`/`../` relative, `~/` home, a Windows
// drive (`C:\`/`C:/`), or a `\\` UNC path. A bare slash-bearing word like
// `and/or` or `docs/setup` is NOT a path — that's prose.
function isPathLike(tok: string): boolean {
  return /^(\/|\.\.?\/|~\/|[A-Za-z]:[\\/]|\\\\)/.test(tok);
}

// A token worth cloning (vs. a prose label like `Repos:`, a slashed word like
// `and/or`, or a markdown bullet `-`/`*` to drop when a line is split): a hard
// source, a real path, or a bare `name.git`.
function looksLikeSource(tok: string): boolean {
  return isHardSource(tok) || isPathLike(tok) || /\.git$/i.test(tok);
}

/**
 * Parse a paste box of clone sources into a deduped list, in paste order.
 *
 * Newline always separates sources. Within a line, whitespace/commas/semicolons
 * separate sources ONLY when the line carries a "hard source" token (a scheme
 * `://…` or scp/alias colon-path) that cannot be a fragment of a spaced path —
 * then the line is split: hard sources are kept anywhere, a bare path/`.git`
 * token is kept ONLY as the LAST token (a non-final path token is usually a
 * fragment of a spaced local path we'd otherwise truncate, or prose), and the
 * rest (labels like `Repos:`, bullets `-`/`*`, words like `and/or`) is dropped.
 * A line without that evidence is taken as ONE source, so a local path with
 * spaces (`/Users/me/My Projects/repo`) survives; spaced paths sharing a line
 * with a URL aren't supported — put them on their own line.
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
    // Wrapper-trim each token BEFORE the source-shape checks, or a wrapped source
    // (`` `gh:org/repo.git` ``) would fail the predicate and be dropped.
    const tokens = line
      .split(/[\s,;]+/)
      .map(trimUrl)
      .filter(Boolean);
    if (tokens.length > 1 && tokens.some(isHardSource)) {
      tokens.forEach((tok, i) => {
        // hard sources anywhere; a bare path/.git token only as the LAST token —
        // a non-final one is likely a spaced-path fragment (don't truncate) or prose.
        if (isHardSource(tok) || (looksLikeSource(tok) && i === tokens.length - 1)) add(tok);
      });
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
