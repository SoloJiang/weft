import type { LeadMessage } from "../lib/types";

function deliveryOrder(row: LeadMessage): number {
  return row.seq ?? row.id;
}

export function orderLeadMessages(rows: LeadMessage[]): LeadMessage[] {
  return [...rows].sort((a, b) => deliveryOrder(a) - deliveryOrder(b) || a.id - b.id);
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
      ...(content != null ? { content: JSON.stringify({ text: content }) } : {}),
      ...(seq != null ? { seq } : {}),
    };
  });
  if (seq == null) return updated;
  return orderLeadMessages(updated);
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
