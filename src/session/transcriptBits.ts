// Shared tool-event presentation bits for the transcript views (the legacy
// jsonl-projection Transcript and the chat-engine ChatTimeline).

import type { ComponentType } from "react";
import {
  FilePen,
  FileText,
  ListTodo,
  type LucideProps,
  Radio,
  Search,
  SquareTerminal,
  Wrench,
} from "lucide-react";
import { isPathLike } from "../lib/filePathParsing.ts";

const EXTENDED_FILE_TARGET_RE =
  /(?:^|[\s"'`])((?:~[\\/]|(?:[A-Za-z]:[\\/])?)[^\s"'`,;!?，。；！？、]+?\.[A-Za-z0-9][\w-]*(?::\d+(?::\d+)?)?)(?:$|[\s"'`),.;:!?，。；！？、])/;
const MANIFEST_FILE_TARGET_RE =
  /(?:^|[\s"'`])((?:~[\\/]|(?:[A-Za-z]:[\\/])?)[^\s"'`,;!?，。；！？、]*(?:Dockerfile|Makefile|\.gitignore|\.env(?:\.[\w.-]+)?))(?:$|[\s"'`),.;:!?，。；！？、])/;
const PATH_SEP_TARGET_RE =
  /(?:^|[\s"'`])((?:~[\\/]|(?:[A-Za-z]:[\\/])?)[^\s"'`,;!?，。；！？、]+[\\/][^\s"'`,;!?，。；！？、]+)(?:$|[\s"'`),.;:!?，。；！？、])/;
type ToolKind = "command" | "edit" | "search" | "read" | "sync" | "todo" | "generic";

/** Map a (cleaned) tool name to a glyph so the pills are scannable. */
export function toolIcon(name: string): ComponentType<LucideProps> {
  const icons: Record<ToolKind, ComponentType<LucideProps>> = {
    command: SquareTerminal,
    edit: FilePen,
    search: Search,
    read: FileText,
    sync: Radio,
    todo: ListTodo,
    generic: Wrench,
  };
  return icons[toolKind(name)];
}

/** Human-scannable tool identity: `mcp__weft_planner__get_task` → "weft_planner · get_task". */
export function cleanToolName(name: string) {
  const mcp = name.match(/^mcp__([^_]+(?:_[^_]+)*?)__(.+)$/);
  if (mcp) return `${mcp[1]} · ${mcp[2]}`;
  return name;
}

/** i18n key for the tool's activity label — call t() on the result. */
export function toolLabelKey(name: string) {
  const labels: Record<ToolKind, string> = {
    command: "session.toolRunning",
    edit: "session.toolEditing",
    search: "session.toolSearching",
    read: "session.toolReading",
    sync: "session.toolSyncing",
    todo: "session.toolOrganizing",
    generic: "session.toolCalling",
  };
  return labels[toolKind(name)];
}

/** Past-tense label for a FINISHED tool row (codex-style "Ran"/"已运行"), vs the
 *  present-continuous `toolLabelKey` the in-flight activity line uses. */
export function toolDoneLabelKey(name: string) {
  const labels: Record<ToolKind, string> = {
    command: "session.toolRan",
    edit: "session.toolEdited",
    search: "session.toolSearched",
    read: "session.toolRead",
    sync: "session.toolSynced",
    todo: "session.toolOrganized",
    generic: "session.toolCalled",
  };
  return labels[toolKind(name)];
}

/** Squeeze a tool call's target into a compact, scannable fragment. */
export function compactToolTarget(name: string, summary: string) {
  const raw = summary || name;
  const file = extractToolFileTarget(name, raw);
  const target = file
    ? file.split(/[\\/]/).slice(-2).join("/")
    : raw.replace(/\s+/g, " ").slice(0, 90);
  const targetToken = file;
  const added = raw.match(/(?:\+|added[:= ]+)(\d+)/i)?.[1];
  const removed = raw.match(/(?:-|removed[:= ]+)(\d+)/i)?.[1];
  return { target, targetToken, added, removed };
}

function extractToolFileTarget(name: string, raw: string): string | undefined {
  if (isSearchTool(name)) return undefined;
  if (isCommandTool(name)) return undefined;
  if (!toolAllowsFileTarget(name)) return undefined;
  const file = matchToolPath(raw, EXTENDED_FILE_TARGET_RE);
  if (file && !isWebUrl(file) && isPathLike(file)) return file;
  const manifest = matchToolPath(raw, MANIFEST_FILE_TARGET_RE);
  if (manifest && !isWebUrl(manifest) && isPathLike(manifest)) return manifest;
  if (!allowsSlashOnlyToolTarget(name)) return undefined;
  const sepTarget = matchToolPath(raw, PATH_SEP_TARGET_RE);
  if (!sepTarget) return undefined;
  if (isWebUrl(sepTarget)) return undefined;
  if (isPathLike(sepTarget)) return sepTarget;
  return /^(?:~[\\/]|(?:[A-Za-z]:[\\/])?)[^\s"'`,;!?，。；！？、]+[\\/][^\s"'`,;!?，。；！？、]+$/.test(sepTarget)
    ? sepTarget
    : undefined;
}

export function toolAllowsFileTarget(name: string): boolean {
  const kind = toolKind(name);
  return kind === "read" || kind === "edit" || kind === "search";
}

function matchToolPath(raw: string, pattern: RegExp): string | undefined {
  pattern.lastIndex = 0;
  const match = pattern.exec(raw);
  if (!match) return undefined;
  let file = match[1];
  if (!file) return undefined;
  const captureStart = match.index + match[0].indexOf(file);
  const captureEnd = captureStart + file.length;
  if (isSpacedPathSuffix(raw, captureStart)) return undefined;
  if (isSpacedPathPrefix(raw, captureEnd)) return undefined;
  file = unwrapParenthesizedToolPath(raw, file, captureEnd);
  return file || undefined;
}

function isSpacedPathSuffix(raw: string, captureStart: number): boolean {
  const space = captureStart - 1;
  if (space < 0 || !/\s/.test(raw[space] ?? "")) return false;
  const before = raw.slice(0, space);
  const previousToken = before.match(/[^\s"'`]+$/)?.[0] ?? "";
  return /[/\\]/.test(previousToken);
}

function isSpacedPathPrefix(raw: string, captureEnd: number): boolean {
  if (!/\s/.test(raw[captureEnd] ?? "")) return false;
  const nextToken = raw.slice(captureEnd).trimStart().match(/^[^\s"'`]+/)?.[0] ?? "";
  return /[/\\]/.test(nextToken) || /\.[A-Za-z0-9]+(?::\d+(?::\d+)?)?$/.test(nextToken);
}

function unwrapParenthesizedToolPath(raw: string, file: string, captureEnd: number): string {
  if (!file.startsWith("(")) return file;
  return raw[captureEnd] === ")" ? file.slice(1) : file;
}

function isWebUrl(value: string): boolean {
  return /^[a-z][a-z0-9+.-]*:\/\//i.test(value);
}

function allowsSlashOnlyToolTarget(name: string): boolean {
  const kind = toolKind(name);
  return kind === "read" || kind === "edit" || kind === "search";
}

function isSearchTool(name: string): boolean {
  const tokens = toolNameTokens(name);
  return tokens.some((token) =>
    token === "grep" || token === "glob" || token === "rg" || token === "ripgrep" || token === "find" || token === "search"
  );
}

function isCommandTool(name: string): boolean {
  return toolKind(name) === "command";
}

function toolNameTokens(name: string): string[] {
  return name
    .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .filter(Boolean);
}

function toolKind(name: string): ToolKind {
  const tokens = toolNameTokens(name);
  const has = (token: string) => tokens.includes(token);
  if (has("write") || has("edit") || has("patch")) return "edit";
  if (has("filechange") || (has("file") && has("change"))) return "edit";
  if (has("apply") && has("patch")) return "edit";
  if (has("grep") || has("glob") || has("rg") || has("ripgrep") || has("find") || has("search")) {
    return "search";
  }
  if (has("ls") || has("list")) return "search";
  if (has("read") || has("view") || has("cat")) return "read";
  if (has("bash") || has("command") || has("exec") || has("shell") || has("run")) return "command";
  if (
    has("bus") ||
    has("broadcast") ||
    (has("ask") && has("human")) ||
    has("announce") ||
    has("interface") ||
    has("inbox") ||
    has("status")
  ) {
    return "sync";
  }
  if (has("todo")) return "todo";
  return "generic";
}
