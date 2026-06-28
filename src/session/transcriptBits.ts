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
  // `command`/`file_change` cover codex's item types in both dialects: exec's
  // snake_case (command_execution/file_change) and app-server's camelCase
  // (commandExecution/fileChange, lowercased here).
  if (/(bash|command|exec_command|shell|run)/.test(n)) return SquareTerminal;
  if (/(write|edit|apply_patch|patch|file_change|filechange)/.test(n)) return FilePen;
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
  if (/(write|edit|apply_patch|patch|file_change|filechange)/.test(n)) return "session.toolEditing";
  if (/(read|view|cat)/.test(n)) return "session.toolReading";
  if (/(grep|glob|rg|ripgrep|ls|find|list|search)/.test(n)) return "session.toolSearching";
  if (/(bash|command|exec_command|shell|run)/.test(n)) return "session.toolRunning";
  if (/(bus_|broadcast|ask_human|announce|interface|inbox|status)/.test(n)) return "session.toolSyncing";
  if (/todo/.test(n)) return "session.toolOrganizing";
  return "session.toolCalling";
}

/** Past-tense label for a FINISHED tool row (codex-style "Ran"/"已运行"), vs the
 *  present-continuous `toolLabelKey` the in-flight activity line uses. */
export function toolDoneLabelKey(name: string) {
  const n = name.toLowerCase();
  if (/(write|edit|apply_patch|patch|file_change|filechange)/.test(n)) return "session.toolEdited";
  if (/(read|view|cat)/.test(n)) return "session.toolRead";
  if (/(grep|glob|rg|ripgrep|ls|find|list|search)/.test(n)) return "session.toolSearched";
  if (/(bash|command|exec_command|shell|run)/.test(n)) return "session.toolRan";
  if (/(bus_|broadcast|ask_human|announce|interface|inbox|status)/.test(n)) return "session.toolSynced";
  if (/todo/.test(n)) return "session.toolOrganized";
  return "session.toolCalled";
}

/** Squeeze a tool call's target into a compact, scannable fragment. */
export function compactToolTarget(name: string, summary: string) {
  const raw = summary || name;
  const file =
    raw.match(
      /(?:^|[\s"'`])([\w./\-@[\]]+\.(?:tsx|ts|jsx|js|rs|css|md|json|toml|yaml|yml|html))(?:$|[\s"'`),.;:!?])/,
    )?.[1] ??
    raw.match(/(?:^|[\s"'`])([\w./\-@[\]]+\/[\w./\-@[\]]+)(?:$|[\s"'`),.;:!?])/)?.[1];
  const target = file ? file.split("/").slice(-2).join("/") : raw.replace(/\s+/g, " ").slice(0, 90);
  const targetToken = file;
  const added = raw.match(/(?:\+|added[:= ]+)(\d+)/i)?.[1];
  const removed = raw.match(/(?:-|removed[:= ]+)(\d+)/i)?.[1];
  return { target, targetToken, added, removed };
}
