import type { NeedKind } from "../lib/types";

/** The two NOTICE kinds (everything a `NeedItem` can be except an answerable
 *  `question`) — the subset `AskRow`'s footer actually has to pick a view
 *  for; a `question` renders the answer form instead, never this footer. */
export type NoticeKind = Exclude<NeedKind, "question">;

/** Card accent by `NeedItem.kind` — every kind uses the routine waiting/amber
 *  tone EXCEPT `notice_action_required` (a PR/MR the monitor gave up on:
 *  `host::judge::give_up_text`), which needs to stand out in the LIST, not
 *  just in its own footer text — it gets the same danger tone
 *  `ProcessQuotaBar`'s `degraded` tier uses for "this needs you, not just
 *  FYI". `question` and `notice` intentionally stay IDENTICAL (both routine,
 *  no-special-action-needed cards) — this map's job is to single out the one
 *  kind that differs, not to give every kind its own look. Exhaustive over
 *  `NeedKind` so a future kind can't silently fall back to an unstyled
 *  default. */
export const ASK_CARD_TONE: Record<NeedKind, string> = {
  question: "border-waiting/40 bg-waiting/10",
  notice: "border-waiting/40 bg-waiting/10",
  notice_action_required: "border-danger/40 bg-danger/10",
};

/** Same tone split as `ASK_CARD_TONE`, for the row's small leading status dot. */
export const ASK_DOT_TONE: Record<NeedKind, string> = {
  question: "bg-waiting",
  notice: "bg-waiting",
  notice_action_required: "bg-danger",
};

export interface NoticeFooterView {
  /** "alert" selects the AlertTriangle icon; `null` = no icon. Kept as a
   *  plain string tag rather than the icon component itself so this pure
   *  view-model module has no React/lucide-react dependency (and stays
   *  testable with the plain `node --test` runner, which can't transform
   *  JSX) — `NeedsRows.tsx` maps the tag to the real icon at render time. */
  icon: "alert" | null;
  textKey: string;
  className: string;
}

/** The two NOTICE kinds' footer — replacing the single unconditional
 *  `needs.selfClearing` paragraph `AskRow` used to render for BOTH. That was
 *  the bug this map exists to fix (PR #150 review): a `notice_action_
 *  required` card (the one Needs-you notice that does NOT clear itself) used
 *  to show that exact "clears itself automatically" line directly under a
 *  body that says the opposite. `notice` keeps the PRIOR look byte-for-byte
 *  (no icon, same copy/tone) — only `notice_action_required` is new, so
 *  every other notice in the codebase (the stall hint, the stopped-worker
 *  hint, an ordinary PR/MR update) renders exactly as it did before this
 *  discriminant existed. */
export const NOTICE_FOOTER_VIEW: Record<NoticeKind, NoticeFooterView> = {
  notice: { icon: null, textKey: "needs.selfClearing", className: "text-ink-faint" },
  notice_action_required: {
    icon: "alert",
    textKey: "needs.actionRequired",
    className: "text-danger",
  },
};
