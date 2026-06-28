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
    let rawPath = match[1];
    const originalCaptureStart = start + match[0].indexOf(rawPath);
    const originalCaptureEnd = originalCaptureStart + rawPath.length;
    const originalSpacedPath =
      isSpacedPathSuffix(text, originalCaptureStart) ||
      isSpacedPathPrefix(text, originalCaptureEnd);
    const canTrimEmbeddedPath = !LINE_LABEL_WRAPPERS[rawPath[0] ?? ""];
    const embeddedPath = canTrimEmbeddedPath ? trimLeadingProsePath(rawPath) : "";
    if (!originalSpacedPath && embeddedPath && embeddedPath.length < rawPath.length) {
      rawPath = embeddedPath;
    }
    const boundary = hasLinePathBoundary(text, start + match[0].indexOf(rawPath));
    if (!boundary) rawPath = trimLeadingProsePath(rawPath);
    const captureStart = start + match[0].indexOf(rawPath);
    const captureEnd = captureStart + rawPath.length;
    if (
      !rawPath ||
      originalSpacedPath ||
      isSpacedPathSuffix(text, captureStart) ||
      isSpacedPathPrefix(text, captureEnd)
    ) {
      matched = true;
      const end = start + match[0].length;
      pushText(out, text.slice(last, end));
      last = end;
      continue;
    }
    const lead = lineLabelLeadWrapper(text, start, match[0], rawPath);
    const path = rawPath.slice(lead.length);
    if (!path || !isPathLike(path)) continue;
    matched = true;
    if (start > last) {
      for (const seg of splitPlainTextForPaths(text.slice(last, start))) out.push(seg);
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
  const lead = rawPath[0] ?? "";
  const closing = LINE_LABEL_WRAPPERS[lead];
  if (!closing) return "";
  const next = text[start + matchText.length] ?? "";
  return next === closing ? lead : "";
}

function hasLinePathBoundary(text: string, captureStart: number): boolean {
  if (captureStart <= 0) return true;
  return /[\s"'`([{<【「『〔〈《〖“‘]/.test(text[captureStart - 1] ?? "");
}

function trimLeadingProsePath(rawPath: string): string {
  const rooted = rawPath.match(/(?:src|app|components|pages|jobs|cmd|lib|tests|test|packages|src-tauri)[/\\].*$/);
  return rooted?.[0] ?? "";
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
