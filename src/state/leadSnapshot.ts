import type { LeadMessage } from "../lib/types";

function deliveryOrder(row: LeadMessage): number {
  return row.seq ?? row.id;
}

export function orderLeadMessages(rows: LeadMessage[]): LeadMessage[] {
  return [...rows].sort((a, b) => deliveryOrder(a) - deliveryOrder(b) || a.id - b.id);
}

/**
 * Rebuild a `kind:"text"` row's content with a new `text`, preserving any
 * OTHER top-level field already on it — issue #99's `agentThread` in
 * particular. The backend stamps that tag once at row creation and re-embeds
 * it into every later content rewrite it makes (engine.rs's own
 * `text_row_content` helper does the same merge, on its side); every LIVE
 * content rewrite the frontend makes to the same row (a streamed delta, a
 * finalize push) must do likewise, or the tag would vanish from the live view
 * on the very next rewrite while a cold reload — which re-fetches the row
 * verbatim from the DB — would still show it. That live/cold disagreement is
 * exactly the failure mode this feature's design explicitly rules out.
 * Malformed/non-object existing content (a fresh row with no content yet)
 * degrades to a bare `{text}` object, matching pre-#99 behavior.
 */
export function withText(existingContent: string, text: string): string {
  let rest: Record<string, unknown> = {};
  try {
    const parsed: unknown = JSON.parse(existingContent);
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      rest = parsed as Record<string, unknown>;
    }
  } catch {
    /* not JSON yet (fresh row) — start bare */
  }
  return JSON.stringify({ ...rest, text });
}

/** Apply one engine finalize push atomically. A queued row receives its delivery
 *  seq at the same moment it becomes visible, so it never flashes at the older
 *  enqueue-time position before moving to the end of the completed turn. */
export function applyLeadFinalize(
  rows: LeadMessage[],
  messageId: number,
  status: LeadMessage["status"],
  content?: string,
  seq?: number,
): LeadMessage[] {
  const updated = rows.map((row) => {
    if (row.id !== messageId) return row;
    return {
      ...row,
      status,
      ...(content != null ? { content: withText(row.content, content) } : {}),
      ...(seq != null ? { seq } : {}),
    };
  });
  if (seq == null) return updated;
  return orderLeadMessages(updated);
}

/** Apply a live "consumed" push (issue #94): stamp `consumed_at` on the one
 *  row it names. No reordering — unlike a delivery seq, this never changes a
 *  row's position, only what receipt it renders. */
export function applyLeadConsumed(
  rows: LeadMessage[],
  messageId: number,
  consumedAt: number,
): LeadMessage[] {
  return rows.map((row) => (row.id === messageId ? { ...row, consumed_at: consumedAt } : row));
}

/** A sent message's delivery lifecycle, as the UI needs to distinguish "the
 *  agent never got this" from "the agent has it but hasn't answered yet"
 *  (issue #94). `null` = nothing worth a receipt (queued rows render via
 *  QueueStack instead; a streaming/unknown status has no user-row meaning). */
export type ReceiptState = "delivered" | "consumed" | "interrupted" | "error";

/** Pure classifier — single source of truth for the receipt: every renderer
 *  maps through this instead of re-deriving `status`/`consumed_at` booleans
 *  ad hoc at each call site. */
export function receiptStateOf(m: Pick<LeadMessage, "status" | "consumed_at">): ReceiptState | null {
  if (m.status === "error") return "error";
  if (m.status === "interrupted") return "interrupted";
  if (m.status !== "complete") return null;
  return m.consumed_at != null ? "consumed" : "delivered";
}

function textOf(row: LeadMessage): string | null {
  try {
    const parsed = JSON.parse(row.content) as { text?: unknown };
    return typeof parsed.text === "string" ? parsed.text : null;
  } catch {
    return null;
  }
}

/**
 * Reconcile a freshly fetched thread snapshot with the rows already on screen.
 *
 * Streaming text events outrun the backend's ~150ms persist throttle, so while
 * a row is still streaming IN THE SNAPSHOT the locally accumulated text is the
 * fresher value whenever it extends the snapshot's as a prefix — taking the
 * snapshot verbatim would truncate the live transcript until the next reload
 * (finalize usually carries status only, not the body). Everything else comes
 * from the snapshot: rows with no local counterpart, rows the snapshot already
 * finalized (including cleaned bodies that no longer prefix-match), and rows
 * whose local text diverged. Local-only rows are dropped, matching the
 * pre-coalescing "a snapshot supersedes streaming state" semantics.
 */
export function mergeLeadSnapshot(
  local: LeadMessage[],
  snapshot: LeadMessage[],
): LeadMessage[] {
  const localById = new Map(local.map((x) => [x.id, x]));
  return snapshot
    .filter((x) => x.kind !== "meta")
    .map((snap) => {
      if (snap.status !== "streaming") return snap;
      const cur = localById.get(snap.id);
      if (!cur) return snap;
      const curText = textOf(cur);
      const snapText = textOf(snap);
      if (curText == null || snapText == null) return snap;
      // `cur` may already be finalized locally (the finalize event beat the
      // snapshot read) — keeping it preserves both the fuller text AND the
      // settled status.
      return curText.length > snapText.length && curText.startsWith(snapText) ? cur : snap;
    });
}
