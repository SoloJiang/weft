export function parsePathToken(
  token: string,
  isUrl = false,
): { path: string; line?: number; col?: number } {
  let t = token.trim();
  if (isUrl) {
    const scheme = t.match(/^file:(?:\/\/)?/i);
    if (scheme) t = t.slice(scheme[0].length).replace(/^localhost(?=\/)/i, "");
    const frag = t.search(/[#?]/);
    if (frag !== -1) t = t.slice(0, frag);
    try {
      t = decodeURIComponent(t);
    } catch {
      /* malformed %-escape — leave as-is */
    }
    t = t.replace(/^\/([A-Za-z]:[\\/])/, "$1");
  }
  const m = t.match(/^(.*?):(\d+)(?::(\d+))?$/);
  if (m && m[1].length > 1) {
    return { path: m[1], line: Number(m[2]), col: m[3] ? Number(m[3]) : undefined };
  }
  return { path: t };
}

const CODE_EXT =
  "tsx?|jsx?|mjs|cjs|json|jsonc|rs|toml|lock|md|mdx|css|scss|less|html?|py|go|rb|php|java|kt|kts|swift|c|cc|cpp|cxx|h|hpp|m|mm|sh|bash|zsh|fish|ya?ml|sql|svg|vue|svelte|astro|txt|env|ini|cfg|conf|xml|gradle|proto|graphql|prisma";
const MANIFEST =
  "package(?:-lock)?\\.json|pnpm-lock\\.yaml|yarn\\.lock|tsconfig(?:\\.[\\w.-]+)?\\.json|Cargo\\.(?:toml|lock)|Dockerfile|Makefile|\\.gitignore|\\.env(?:\\.[\\w.-]+)?";
const EXT_RE = new RegExp(`\\.(?:${CODE_EXT})$`, "i");
const MANIFEST_RE = new RegExp(`(?:^|[/\\\\])(?:${MANIFEST})$`);

export function isPathLike(token: string, allowSpaces = false): boolean {
  const { path } = parsePathToken(token);
  if (!path) return false;
  if (/\s/.test(path)) {
    if (!allowSpaces || !/^(\/|~[\\/]?|\.\.?[\\/]|[a-zA-Z]:[\\/])/.test(path)) return false;
  }
  if (/^(\/|\\|~[\\/]?|\.\.?[\\/])/.test(path)) return true;
  if (/^[a-zA-Z]:[\\/]/.test(path)) return true;
  if (EXT_RE.test(path)) return true;
  if (MANIFEST_RE.test(path)) return true;
  return false;
}

export type Seg =
  | { type: "text"; value: string }
  | { type: "path"; value: string; label?: string };

const CJK_PUNCT = "（）【】「」『』〔〕〈〉《》〖〗。，、；：！？…—～·｜“”‘’";
const LIST_SEP = ",;";
const DELIM = new RegExp(`(\\s+|[${LIST_SEP}${CJK_PUNCT}]+)`);
const IS_DELIM = new RegExp(`^(?:\\s|[${LIST_SEP}${CJK_PUNCT}])`);
const LEAD_PUNCT = /^[([{<'"`]+/;
const TAIL_PUNCT = /[)\]}>'"`.,;:!?]+$/;
const PATH_WITH_LINE_RE = new RegExp(
  `([^\\s"'()（）]+\\.(?:${CODE_EXT}))\\s+\\(line\\s+(\\d+)(?:\\s*,\\s*(?:col|column)\\s+(\\d+))?\\)`,
  "gi",
);

function couldBePath(core: string): boolean {
  if (/[/\\.]/.test(core)) return true;
  const head = core.replace(/:\d+(?::\d+)?$/, "");
  return head === "Dockerfile" || head === "Makefile";
}

function pushText(segs: Seg[], v: string): void {
  const last = segs[segs.length - 1];
  if (last && last.type === "text") last.value += v;
  else segs.push({ type: "text", value: v });
}

export function splitTextForPaths(text: string): Seg[] {
  const withLines = splitLineLabels(text);
  if (withLines) return withLines;
  return splitPlainTextForPaths(text);
}

function splitLineLabels(text: string): Seg[] | null {
  PATH_WITH_LINE_RE.lastIndex = 0;
  const out: Seg[] = [];
  let last = 0;
  let matched = false;
  for (const match of text.matchAll(PATH_WITH_LINE_RE)) {
    const start = match.index ?? 0;
    const path = match[1];
    if (!path || !isPathLike(path)) continue;
    matched = true;
    if (start > last) {
      for (const seg of splitPlainTextForPaths(text.slice(last, start))) out.push(seg);
    }
    const line = match[2];
    const col = match[3];
    const value = col ? `${path}:${line}:${col}` : `${path}:${line}`;
    out.push({ type: "path", value, label: match[0] });
    last = start + match[0].length;
  }
  if (!matched) return null;
  if (last < text.length) {
    for (const seg of splitPlainTextForPaths(text.slice(last))) out.push(seg);
  }
  return out;
}

function splitPlainTextForPaths(text: string): Seg[] {
  const segs: Seg[] = [];
  for (const part of text.split(DELIM)) {
    if (!part) continue;
    if (IS_DELIM.test(part)) {
      pushText(segs, part);
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
