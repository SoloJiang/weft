/** One pending append per streaming message, flushed as a single batch. */
export type DeltaBatch = Array<{ threadId: number; messageId: number; text: string }>;

type Schedule = (flush: () => void) => () => void;

/** Default scheduler: one flush per animation frame. */
const rafSchedule: Schedule = (flush) => {
  const id = window.requestAnimationFrame(flush);
  return () => window.cancelAnimationFrame(id);
};

/**
 * Coalesces `lead-chat` delta pushes. The engine forwards every raw token
 * chunk unthrottled, and applying each one straight to React state re-renders
 * the transcript (and re-parses the streaming message's markdown) per chunk.
 * Buffering the appends and flushing once per animation frame caps that work
 * at the paint rate without adding any perceptible latency.
 *
 * Ordering contract: `flushNow()` must run before any NON-delta event is
 * applied. Finalize can replace the streamed body with authoritative content
 * and tool rows rely on row order, so pending appends have to land first —
 * with that rule, coalesced processing is indistinguishable from the previous
 * one-event-one-setState behavior (per-message appends concatenate in arrival
 * order; appends to different messages commute).
 */
export function createDeltaCoalescer(
  flush: (batch: DeltaBatch) => void,
  schedule: Schedule = rafSchedule,
) {
  const pending = new Map<string, { threadId: number; messageId: number; text: string }>();
  // `scheduled` is the gate (not the canceller): a scheduler that fires
  // synchronously would otherwise leave a stale canceller behind and block
  // every later flush.
  let scheduled = false;
  let cancel: (() => void) | null = null;

  const drain = () => {
    scheduled = false;
    cancel = null;
    if (pending.size === 0) return;
    const batch = [...pending.values()];
    pending.clear();
    flush(batch);
  };

  return {
    /** Buffer one delta; schedules a flush if none is pending. */
    push(threadId: number, messageId: number, text: string): void {
      const key = `${threadId}|${messageId}`;
      const entry = pending.get(key);
      if (entry) {
        entry.text += text;
      } else {
        pending.set(key, { threadId, messageId, text });
      }
      if (!scheduled) {
        scheduled = true;
        cancel = schedule(drain);
      }
    },
    /** Drain synchronously (cancelling any scheduled flush). */
    flushNow(): void {
      cancel?.();
      drain();
    },
  };
}
