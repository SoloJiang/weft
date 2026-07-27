// Groups the flat ChatTimeline row stream into top-level rows plus collapsible
// sub-agent branches (issue #99: "子 agent 输出折叠成支线").
//
// v2 — replaces the frontend-only heuristic from PR #132, which an independent
// review BLOCKED (see the PR's review thread + weft issue #99). That version
// inferred "which rows belong to a collab call" from OBSERVED ARRIVAL ORDER: a
// row first seen while a `collabAgentToolCall` anchor was "streaming" was
// assumed to be its child, on the premise that the caller can't emit further
// rows of its own while blocked on a pending call. The reviewer falsified that
// premise with the engine's own code and a reproduced counterexample: codex
// app-server demuxes a collab sub-agent's parallel activity into the SAME row
// stream as the main narration (see engine.rs's `open_texts` doc, added for
// #85's stream demux), so the lead's own unrelated tool calls routinely arrive
// WHILE a collab call is still open — and got mis-folded into its branch,
// silently hiding real mainline activity. That's the opposite of what #99
// exists to fix.
//
// This version groups on an explicit backend signal instead of guessing from
// timing. Two minimal fields now ride in a row's persisted `content` JSON
// (src-tauri/src/lead_chat/engine.rs, codex_app_server.rs — both existing
// app-server wire fields the engine previously read and discarded):
//   - `agentThread`: which sub-agent thread produced THIS row (absent = the
//     lead's own mainline activity — including every row from a dialect that
//     doesn't have this concept at all).
//   - `collabThreads` (only on a `collabAgentToolCall` tool row): which
//     sub-agent thread id(s) THIS call knows about (app-server's
//     `receiverThreadIds`), filled in as soon as the call resolves enough to
//     know them — empty at a spawn's `item/started`, populated by its
//     `item/completed`; already populated at `item/started` for a send/wait.
// Only codex app-server ever sets these: codex exec recognizes the SAME tool
// by name but its transport has no per-event thread id for any later row to
// carry (see engine.rs's doc on `codex_tool_call`), and claude/opencode have
// no collab/sub-agent concept at all. Every row from those dialects is simply
// left untagged, which this module already treats as "top-level" — grouping
// degrades to exactly today's flat rendering automatically, with no
// per-dialect branching anywhere in this file.
//
// Because both signals are PERSISTED (not just observed live), this is a pure,
// STATELESS function of the current row list — no ref-persisted tracker, no
// "sticky" decisions, no rewind/timelineKey reset handling needed (unlike
// PR #132's `useCollabBranches` hook): the same rows always group the same
// way, whether replayed cold from history or watched live, because grouping
// never depends on WHEN a row was observed, only on the ids already baked
// into the data.

import type { LeadMessage } from "../lib/types";

/**
 * A flattened chat row, or a sub-agent thread's branch: the row that first
 * revealed that thread id (its "anchor") with everything tagged to it —
 * including any LATER `collabAgentToolCall` row addressing the same thread
 * (a second `wait`/`send` call) — nested underneath, recursively (a sub-agent
 * that itself spawns a sub-sub-agent nests one level deeper, naturally).
 */
export type TimelineNode =
  | { kind: "row"; row: LeadMessage }
  | { kind: "branch"; anchor: LeadMessage; children: TimelineNode[] };

/** Stable Virtuoso item key: the anchor's row id for a branch (identical to
 *  what it rendered as before it grew children), the row's own id otherwise —
 *  both spaces share the same globally-unique LeadMessage ids. */
export function nodeKey(node: TimelineNode): number {
  return node.kind === "row" ? node.row.id : node.anchor.id;
}

/** Every row a branch tree renders at the TOP level: a plain row as itself, a
 *  branch as its anchor row (never its children) — exactly what the reader
 *  sees as separate items before expanding anything. Feed this, not the raw
 *  flat list, to any "is this the newest thing on screen" positional check
 *  (isLastAssistant / isTail / isLatestProposal / hasPendingUserReply) — a row
 *  the reader would have to expand a collapsed branch to even see must never
 *  count as "newer" than a still-actionable top-level card (review [P1]). */
export function topLevelRows(nodes: TimelineNode[]): LeadMessage[] {
  return nodes.map((n) => (n.kind === "row" ? n.row : n.anchor));
}

/** Kinds the backend can EVER tag with `agentThread` — see engine.rs: only a
 *  `kind:"text"` row (an app-server item-keyed streamed row) or a `kind:"tool"`
 *  row (`persist_tool_calls`) carries it. An EXHAUSTIVE `Record` (not an
 *  allowlist `Set`) per this repo's discriminated-state convention (CLAUDE.md)
 *  — adding a new `LeadMessage["kind"]` is a compile error here until it's
 *  classified, instead of silently defaulting to "never nests" or "always
 *  nests". Defense in depth: even if a future backend change accidentally
 *  stamped `agentThread` on some other kind, this stays the hard backstop that
 *  keeps a decision card / rewind marker / settled trail always top-level. */
const NESTABLE_KIND: Record<LeadMessage["kind"], boolean> = {
  text: true,
  tool: true,
  command: false,
  proposal: false,
  approval: false,
  worker_event: false,
  meta: false,
  action_card: false,
  plan_card: false,
  test_cases: false,
  settled: false,
  rewind: false,
  // Same system-owned, always-top-level marker treatment as `rewind` (issue
  // #96/#98) — see EngineSwitchMarker in ChatTimeline.tsx.
  engine_switch: false,
  // Same treatment — a failed auto fail-over attempt (issue #97) is also a
  // system-owned marker, never a per-agent row.
  quota_failover_failed: false,
  // Routing decisions and blocked routing outcomes are durable system-owned
  // markers, not conversation rows that belong inside a collab branch.
  engine_route: false,
  engine_route_blocked: false,
};

function parseContentObject(row: LeadMessage): Record<string, unknown> {
  try {
    const v: unknown = JSON.parse(row.content);
    return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : {};
  } catch {
    return {};
  }
}

/** This row's own sub-agent origin (`agentThread`), or null for the lead's own
 *  mainline activity. Guarded by role AND `NESTABLE_KIND` — the backend only
 *  ever stamps this on an assistant-authored text/tool row, never a user row,
 *  but checking both here costs nothing and means this module never trusts
 *  content shape alone to decide something as visible as "is this hidden". */
function agentThreadOf(row: LeadMessage): string | null {
  if (row.role !== "assistant" || !NESTABLE_KIND[row.kind]) return null;
  const t = parseContentObject(row).agentThread;
  return typeof t === "string" && t ? t : null;
}

/** `collabAgentToolCall` row names across both codex dialects — app-server's
 *  camelCase (codex_app_server.rs) and exec's snake_case (lead_chat/proto.rs).
 *  Exec never populates `collabThreads` (see the module doc), so matching its
 *  name here is harmless, not load-bearing — kept for exact parity with how
 *  engine.rs recognizes the same tool. */
const COLLAB_TOOL_NAMES = new Set(["collabAgentToolCall", "collab_agent_tool_call"]);

/** A `collabAgentToolCall` row's known sub-agent thread ids (`collabThreads`),
 *  or an empty array for any other row — including a collab call that hasn't
 *  resolved enough to know its target yet (e.g. a spawn's `item/started`). */
function collabThreadsOf(row: LeadMessage): string[] {
  if (row.kind !== "tool") return [];
  const content = parseContentObject(row);
  if (typeof content.name !== "string" || !COLLAB_TOOL_NAMES.has(content.name)) return [];
  const threads = content.collabThreads;
  return Array.isArray(threads) ? threads.filter((t): t is string => typeof t === "string") : [];
}

/**
 * Groups a flat LeadMessage timeline into top-level rows plus collapsible
 * sub-agent branches (issue #99). Deterministic and stateless: every thread id
 * anchors to the EARLIEST row (lowest id) that ever reveals it. That row is
 * always the earliest structurally possible one — a `collabAgentToolCall` row
 * announcing a call is inserted before that call's target can exist, let alone
 * produce its own rows — so a two-pass, whole-array scan never has to guess
 * ahead or revise a decision once made:
 *
 *   Pass 1 finds each thread id's anchor row (the first row whose
 *   `collabThreads` mentions it).
 *   Pass 2 places every row: its OWN `agentThread` wins first (a sub-agent's
 *   own text/tool row nests under ITS thread's anchor); else, a
 *   `collabAgentToolCall` row referencing an ALREADY-anchored thread nests
 *   under that earlier anchor too (a second `wait`/`send` call addressing the
 *   same sub-agent joins its existing branch instead of opening a sibling
 *   one); else the row is top-level — and if it introduces a brand-new thread
 *   id, IT becomes that thread's anchor for anything found in pass 1.
 *
 * `NESTABLE_KIND` (via `agentThreadOf`) is a hard backstop under all of this:
 * a row outside text/tool, or with role other than "assistant", can never
 * nest, however its content happens to be shaped.
 */
export function groupTimeline(rows: LeadMessage[]): TimelineNode[] {
  const anchorRowId = new Map<string, number>();
  for (const row of rows) {
    for (const t of collabThreadsOf(row)) {
      if (!anchorRowId.has(t)) anchorRowId.set(t, row.id);
    }
  }

  const parentOf = new Map<number, number | null>();
  for (const row of rows) {
    const own = agentThreadOf(row);
    const ownAnchor = own != null ? anchorRowId.get(own) : undefined;
    if (ownAnchor != null && ownAnchor !== row.id) {
      parentOf.set(row.id, ownAnchor);
      continue;
    }
    let parent: number | null = null;
    for (const t of collabThreadsOf(row)) {
      const anchor = anchorRowId.get(t);
      if (anchor != null && anchor !== row.id) {
        parent = anchor;
        break;
      }
    }
    parentOf.set(row.id, parent);
  }

  return buildForest(rows, parentOf);
}

function buildForest(
  rows: LeadMessage[],
  parentOf: Map<number, number | null>,
): TimelineNode[] {
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
    // childrenOf only ever gets an entry for an id some OTHER row named as its
    // parent, which (see groupTimeline) only ever happens via an anchor —
    // a non-empty `kids` therefore already implies `row` is one.
    return kids && kids.length > 0
      ? { kind: "branch", anchor: row, children: kids.map(toNode) }
      : { kind: "row", row };
  };
  return roots.map(toNode);
}
