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

/** Map a (cleaned) tool name to a glyph so the pills are scannable. */
export function toolIcon(name: string): ComponentType<LucideProps> {
  const n = name.toLowerCase();
  // `command_execution` / `file_change` are codex's item-type names; keep them in
  // step with claude/opencode tool names so codex tool rows get the same glyphs.
  if (/(bash|exec_command|command_execution|shell|run)/.test(n)) return SquareTerminal;
  if (/(write|edit|apply_patch|patch|file_change)/.test(n)) return FilePen;
  if (/(grep|glob|rg|ripgrep|ls|find|list)/.test(n)) return Search;
  if (/read|view|cat/.test(n)) return FileText;
  if (/(bus_|broadcast|ask_human|announce|interface|inbox|status)/.test(n)) return Radio;
  if (/todo/.test(n)) return ListTodo;
  return Wrench;
}

/** Human-scannable tool identity: `mcp__weft_planner__get_task` → "weft_planner · get_task". */
export function cleanToolName(name: string) {
  const mcp = name.match(/^mcp__([^_]+(?:_[^_]+)*?)__(.+)$/);
  if (mcp) return `${mcp[1]} · ${mcp[2]}`;
  return name;
}

/** i18n key for the tool's activity label — call t() on the result. */
export function toolLabelKey(name: string) {
  const n = name.toLowerCase();
  if (/(write|edit|apply_patch|patch|file_change)/.test(n)) return "session.toolEditing";
  if (/(read|view|cat)/.test(n)) return "session.toolReading";
  if (/(grep|glob|rg|ripgrep|ls|find|list|search)/.test(n)) return "session.toolSearching";
  if (/(bash|exec_command|command_execution|shell|run)/.test(n)) return "session.toolRunning";
  if (/(bus_|broadcast|ask_human|announce|interface|inbox|status)/.test(n)) return "session.toolSyncing";
  if (/todo/.test(n)) return "session.toolOrganizing";
  return "session.toolCalling";
}

/** Squeeze a tool call's target into a compact, scannable fragment. */
export function compactToolTarget(name: string, summary: string) {
  const raw = summary || name;
  const file =
    raw.match(/(?:^|[\\s"'`])([\\w./-]+\\.(?:tsx|ts|jsx|js|rs|css|md|json|toml|yaml|yml|html))/)?.[1] ??
    raw.match(/(?:^|[\\s"'`])([\\w./-]+\/[\\w./-]+)/)?.[1];
  const target = file ? file.split("/").slice(-2).join("/") : raw.replace(/\s+/g, " ").slice(0, 90);
  const added = raw.match(/(?:\+|added[:= ]+)(\d+)/i)?.[1];
  const removed = raw.match(/(?:-|removed[:= ]+)(\d+)/i)?.[1];
  return { target, added, removed };
}
