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
const LINE_LABEL_WRAPPERS: Record<string, string> = {
  "(": ")",
  "[": "]",
  "{": "}",
  "<": ">",
  "'": "'",
  "\"": "\"",
  "`": "`",
  "【": "】",
  "「": "」",
  "『": "』",
  "〔": "〕",
  "〈": "〉",
  "《": "》",
  "〖": "〗",
  "“": "”",
  "‘": "’",
};
const PATH_WITH_LINE_RE = new RegExp(
  `([^\\s"']+?)\\s+\\(line\\s+(\\d+)(?:\\s*,\\s*(?:col|column)\\s+(\\d+))?\\)`,
  "gi",
);
const LINE_LABEL_SYNTAX_RE =
  /\s+\(line\s+\d+(?:\s*,\s*(?:col|column)\s+\d+)?\)$/i;

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

export function hasLineLabelSyntax(text: string): boolean {
  return LINE_LABEL_SYNTAX_RE.test(text);
}

function splitLineLabels(text: string): Seg[] | null {
  PATH_WITH_LINE_RE.lastIndex = 0;
  const out: Seg[] = [];
  let last = 0;
  let matched = false;
  for (const match of text.matchAll(PATH_WITH_LINE_RE)) {
    const start = match.index ?? 0;
    let rawPath = match[1];
    const rawPathOffset = match[0].indexOf(rawPath);
    const originalCaptureStart = start + rawPathOffset;
    const originalCaptureEnd = originalCaptureStart + rawPath.length;
    const originalSpacedPath =
      isSpacedPathSuffix(text, originalCaptureStart) ||
      isSpacedPathPrefix(text, originalCaptureEnd);
    let captureStart = originalCaptureStart;
    const separatedPath = trimLeadingSeparatorPath(rawPath);
    if (!originalSpacedPath && separatedPath && separatedPath.length < rawPath.length) {
      captureStart = originalCaptureStart + rawPath.indexOf(separatedPath);
      rawPath = separatedPath;
    }
    const delimitedPath = trimLeadingDelimitedPath(rawPath);
    if (!originalSpacedPath && delimitedPath && delimitedPath.length < rawPath.length) {
      captureStart = originalCaptureStart + rawPath.indexOf(delimitedPath);
      rawPath = delimitedPath;
    }
    const embeddedPath = trimLeadingProsePath(rawPath);
    if (!originalSpacedPath && embeddedPath && embeddedPath.length < rawPath.length) {
      captureStart += rawPath.indexOf(embeddedPath);
      rawPath = embeddedPath;
    }
    const boundary = hasLinePathBoundary(text, captureStart);
    let rejectedForBoundary = hasLeadingProseBeforeAnchor(rawPath);
    if (!boundary) {
      const trimmedPath = trimLeadingProsePath(rawPath);
      if (trimmedPath) {
        captureStart += rawPath.indexOf(trimmedPath);
        rawPath = trimmedPath;
      } else {
        rejectedForBoundary = true;
      }
    }
    const captureEnd = captureStart + rawPath.length;
    if (
      !rawPath ||
      rejectedForBoundary ||
      originalSpacedPath ||
      isSpacedPathSuffix(text, captureStart) ||
      isSpacedPathPrefix(text, captureEnd)
    ) {
      matched = true;
      const end = start + match[0].length;
      const rejectedStart = originalSpacedPath ? spacedPathStart(text, originalCaptureStart) : start;
      if (rejectedStart > last) {
        for (const seg of splitPlainTextForPaths(text.slice(last, rejectedStart))) out.push(seg);
      }
      pushText(out, text.slice(rejectedStart, end));
      last = end;
      continue;
    }
    const lead = lineLabelLeadWrapper(text, start, match[0], rawPath);
    const path = rawPath.slice(lead.length);
    if (!path || !isPathLike(path)) continue;
    matched = true;
    if (captureStart > last) {
      for (const seg of splitPlainTextForPaths(text.slice(last, captureStart))) out.push(seg);
    }
    if (lead) pushText(out, lead);
    const line = match[2];
    const col = match[3];
    const value = col ? `${path}:${line}:${col}` : `${path}:${line}`;
    out.push({ type: "path", value, label: value });
    last = start + match[0].length;
  }
  if (!matched) return null;
  if (last < text.length) {
    for (const seg of splitPlainTextForPaths(text.slice(last))) out.push(seg);
  }
  return out;
}

function lineLabelLeadWrapper(text: string, start: number, matchText: string, rawPath: string): string {
  let lead = "";
  let rest = rawPath;
  while (rest) {
    const nextLead = rest[0] ?? "";
    const closing = LINE_LABEL_WRAPPERS[nextLead];
    if (!closing) break;
    const nextTail = text[start + matchText.length + lead.length] ?? "";
    if (nextTail !== closing) break;
    lead += nextLead;
    rest = rest.slice(nextLead.length);
  }
  return lead;
}

function hasLinePathBoundary(text: string, captureStart: number): boolean {
  if (captureStart <= 0) return true;
  return /[\s"'`,;:：([{<【「『〔〈《〖“‘]/.test(text[captureStart - 1] ?? "");
}

function trimLeadingSeparatorPath(rawPath: string): string {
  const candidate = rawPath.replace(/^[,;]+/, "");
  return candidate !== rawPath && isPathLike(candidate) ? candidate : "";
}

function trimLeadingDelimitedPath(rawPath: string): string {
  let best = "";
  const matches = rawPath.matchAll(/[：:]/g);
  for (const match of matches) {
    const index = match.index ?? -1;
    if (index < 0) continue;
    const candidate = rawPath.slice(index + match[0].length);
    if (candidate && isPathLike(candidate)) best = candidate;
  }
  return best;
}

function trimLeadingProsePath(rawPath: string): string {
  if (LINE_LABEL_WRAPPERS[rawPath[0] ?? ""]) return "";
  const rooted = rawPath.match(/(?:src|app|components|pages|jobs|cmd|lib|tests|test|packages|src-tauri)[/\\].*$/);
  if (!rooted || rooted.index === undefined) return "";
  const prefix = rawPath.slice(0, rooted.index);
  if (/[A-Za-z0-9_.-]$/.test(prefix)) return "";
  return /[/\\]/.test(prefix) ? "" : rooted[0];
}

function hasLeadingProseBeforeAnchor(rawPath: string): boolean {
  const match = rawPath.match(/^(.*?)(?:\.{1,2}[\\/]|~[\\/]|[A-Za-z]:[\\/])/);
  const prefix = match?.[1] ?? "";
  return prefix.length > 0;
}

function isSpacedPathSuffix(text: string, captureStart: number): boolean {
  const space = captureStart - 1;
  if (space < 0 || !/\s/.test(text[space] ?? "")) return false;
  const before = text.slice(0, space);
  const previousToken = before.match(/[^\s"'`]+$/)?.[0] ?? "";
  return /[/\\]/.test(previousToken);
}

function isSpacedPathPrefix(text: string, captureEnd: number): boolean {
  if (!/\s/.test(text[captureEnd] ?? "")) return false;
  const nextToken = text.slice(captureEnd).trimStart().match(/^[^\s"'`]+/)?.[0] ?? "";
  return /[/\\]/.test(nextToken) || /\.[A-Za-z0-9]+(?::\d+(?::\d+)?)?$/.test(nextToken);
}

function spacedPathStart(text: string, captureStart: number): number {
  const space = captureStart - 1;
  if (space < 0 || !/\s/.test(text[space] ?? "")) return captureStart;
  const before = text.slice(0, space);
  const previousToken = before.match(/[^\s"'`]+$/)?.[0] ?? "";
  return /[/\\]/.test(previousToken) ? space - previousToken.length : captureStart;
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
