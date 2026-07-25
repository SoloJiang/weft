// Groups the flat ChatTimeline row stream into top-level rows plus
// collapsible collab-agent branches (issue #99: "子 agent 输出折叠成支线").

import { useRef } from "react";
import type { LeadMessage } from "../lib/types";

/**
 * A flattened chat row, or a `collabAgentToolCall` anchor with the rows its
 * sub-agent produced while it ran folded underneath it. `ChatTimeline`
 * renders `"row"` nodes exactly as it always has; `"branch"` nodes get a
 * collapsible container (`CollabBranchRow`) built on the same `Tool`
 * expand/collapse primitive PR #19 introduced for ordinary tool rows.
 */
export type TimelineNode =
  | { kind: "row"; row: LeadMessage }
  | { kind: "branch"; anchor: LeadMessage; children: TimelineNode[] };

/** Stable Virtuoso item key: the anchor's row id for a branch (identical to
 *  what the anchor rendered as before grouping existed), the row's own id
 *  otherwise — both spaces share the same globally-unique LeadMessage ids. */
export function nodeKey(node: TimelineNode): number {
  return node.kind === "row" ? node.row.id : node.anchor.id;
}

/** True for a `collabAgentToolCall` row — app-server's camelCase name
 *  (codex_app_server.rs) or exec's snake_case `collab_agent_tool_call`
 *  (lead_chat/proto.rs). Both are persisted as an ordinary `kind:"tool"` row
 *  whose content carries the raw item type as `name`. */
function isCollabAnchor(row: LeadMessage): boolean {
  if (row.kind !== "tool") return false;
  try {
    const name = (JSON.parse(row.content) as { name?: unknown }).name;
    return name === "collabAgentToolCall" || name === "collab_agent_tool_call";
  } catch {
    return false;
  }
}

/** Kinds that are structurally significant enough to never bury inside a
 *  collapsed branch, even if positional evidence would otherwise put them
 *  there — a decision card or a rewind divider going quietly invisible would
 *  raise cognitive load, the opposite of what #99 exists to fix. In practice
 *  the lead can't emit any of these while blocked on its own pending collab
 *  call, so this is a defensive net rather than an observed case. */
const NEVER_NESTED_KINDS = new Set<LeadMessage["kind"]>([
  "proposal",
  "plan_card",
  "action_card",
  "test_cases",
  "settled",
  "rewind",
]);

/** A row that must always render top-level: anything user-authored (a send
 *  mid-turn queues instead of landing live, but this stays a hard guarantee
 *  rather than leaning on that queuing behavior — see the module doc for why
 *  MIS-attributing content is worse than the clutter this issue fixes) plus
 *  the structurally-significant kinds above. */
function alwaysTopLevel(row: LeadMessage): boolean {
  return row.role === "user" || NEVER_NESTED_KINDS.has(row.kind);
}

interface BranchTrackerState {
  timelineKey: string;
  /** High-water mark: rows at or below this id are already classified — see
   *  the module doc's "sticky" note for why that classification never
   *  changes once made. */
  maxSeenId: number;
  /** row id → its branch anchor's row id, or null for a top-level row. */
  parentOf: Map<number, number | null>;
  /** Anchor row ids currently believed still running, nearest (innermost)
   *  last — a row discovered while an anchor is here becomes its child. */
  openStack: number[];
}

function freshState(timelineKey: string): BranchTrackerState {
  return { timelineKey, maxSeenId: -1, parentOf: new Map(), openStack: [] };
}

/**
 * Groups a flat LeadMessage timeline into top-level rows plus collapsible
 * collab-agent branches, with NO backend change: the wire protocol never
 * tells the frontend which rows a collab sub-agent's own activity belongs
 * to — codex app-server demuxes it into the same per-item-keyed row stream
 * as the main narration (see engine.rs's `open_texts` doc, added for #85),
 * just under a different item id that never reaches the UI. The only signal
 * the frontend has is OBSERVED ordering: a collabAgentToolCall row's status
 * flips streaming → complete only once its call genuinely resolves, and by
 * tool-calling protocol semantics the caller can't emit further rows of its
 * own while a pending call blocks it — so a row that FIRST appears while the
 * anchor is still "streaming" is safe to attribute to it.
 *
 * That signal only exists at the moment a row is first observed live. A cold
 * history fetch hands back every row with its FINAL status already settled,
 * so an anchor that was already complete by the time this hook first saw it
 * can't retroactively prove which (if any) trailing rows were its children —
 * guessing would risk folding genuine main-line narration into the wrong
 * branch, which is worse than the clutter this issue exists to fix. Rather
 * than guess, such anchors render exactly as an ordinary tool row (PR #19,
 * unchanged): only anchors CAUGHT mid-flight — observed "streaming" at first
 * sight, whether that's at mount (resuming a live turn) or later while this
 * component stays mounted — grow a reconstructed branch. The more of a
 * conversation you watch live, the more of it groups correctly; cold
 * scrollback degrades to exactly today's flat rendering, never worse.
 *
 * Once a row's parent is decided it never changes ("sticky"), so this
 * id-ordered pass is safe to replay against a growing `messages` array on
 * every render. `parentOf` / `openStack` persist in a ref (not React state)
 * because nothing here needs to trigger its own re-render — the caller
 * already re-renders on every new/changed message.
 */
export function useCollabBranches(rows: LeadMessage[], timelineKey: string): TimelineNode[] {
  const stateRef = useRef<BranchTrackerState>(freshState(timelineKey));
  // A rewind truncates the tail (deletes rows after a point) rather than
  // reordering survivors, so a max id that goes BACKWARD is the tell — start
  // over rather than risk a since-deleted id lingering in openStack, which
  // would silently swallow every row after it (parented to a branch that can
  // never render because its anchor no longer exists in `rows`).
  const regressed = rows.length > 0 && rows[rows.length - 1].id < stateRef.current.maxSeenId;
  if (stateRef.current.timelineKey !== timelineKey || regressed) {
    stateRef.current = freshState(timelineKey);
  }
  const state = stateRef.current;

  for (const row of rows) {
    if (row.id > state.maxSeenId) {
      state.maxSeenId = row.id;
      const parent =
        !alwaysTopLevel(row) && state.openStack.length > 0
          ? state.openStack[state.openStack.length - 1]
          : null;
      state.parentOf.set(row.id, parent);
      if (isCollabAnchor(row) && row.status === "streaming") {
        state.openStack.push(row.id);
      }
      continue;
    }
    // Already classified — but a previously-open anchor may have JUST
    // resolved; once it has, it stops claiming any further rows. openStack
    // only ever holds ids pushed above, so a miss here is the common case
    // (most rows are never anchors) and cheap (openStack is nesting-depth
    // sized, not conversation sized).
    if (state.openStack.length > 0 && row.status !== "streaming") {
      const idx = state.openStack.indexOf(row.id);
      if (idx !== -1) state.openStack.length = idx;
    }
  }

  return buildForest(rows, state.parentOf);
}

function buildForest(rows: LeadMessage[], parentOf: Map<number, number | null>): TimelineNode[] {
  const childrenOf = new Map<number, LeadMessage[]>();
  const roots: LeadMessage[] = [];
  for (const row of rows) {
    const parent = parentOf.get(row.id) ?? null;
    if (parent == null) {
      roots.push(row);
      continue;
    }
    const list = childrenOf.get(parent);
    if (list) list.push(row);
    else childrenOf.set(parent, [row]);
  }
  const toNode = (row: LeadMessage): TimelineNode => {
    const kids = childrenOf.get(row.id);
    // childrenOf only ever gets an entry for an id that was pushed onto
    // openStack, which only ever happens for a confirmed collab anchor — a
    // non-empty `kids` therefore already implies `row` is one.
    return kids && kids.length > 0
      ? { kind: "branch", anchor: row, children: kids.map(toNode) }
      : { kind: "row", row };
  };
  return roots.map(toNode);
}
