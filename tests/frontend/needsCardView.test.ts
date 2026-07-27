import test from "node:test";
import assert from "node:assert/strict";
import { ASK_CARD_TONE, ASK_DOT_TONE, NOTICE_FOOTER_VIEW } from "../../src/board/needsCardView.ts";

// PR #150 review (P2): AskRow used to render ONE unconditional footer
// ("clears itself automatically") for every non-answerable NeedItem, even the
// PR/MR give-up notice whose own body text says the opposite. These tests
// pin both halves of the fix: existing notices/questions keep the EXACT prior
// look (regression guard), and the new `notice_action_required` kind is both
// visually and textually distinct (the actual fix).
//
// Regression scope, by NeedKind (mirrors bus::AskKind 1:1 — see NeedItem.kind
// in src/lib/types.ts): "question" is an answerable ask (the human's own
// question, or a tool-permission-style prompt) — renders the input form, no
// footer at all, untouched by this map. "notice" is every SELF-CLEARING
// backend notice as of this PR: the task-stall hint (`lead_chat::engine::
// stall_notice_text`), the stopped-worker hint (`lead_chat::revive::
// stopped_worker_notice_text`), and an ordinary PR/MR readiness/probe-error
// update (`host::judge::notice_text` / `probe_error_text`) — all three post
// via `BusRegistry::notify_human` and must keep rendering EXACTLY the footer
// below. Only `notice_action_required` (the PR/MR give-up notice, posted via
// the new `notify_human_action_required`) is new.

test("question and notice share the identical card tone — unaffected by this change", () => {
  assert.equal(ASK_CARD_TONE.question, ASK_CARD_TONE.notice);
  assert.equal(ASK_DOT_TONE.question, ASK_DOT_TONE.notice);
});

test("notice_action_required gets a visually distinct card and dot tone", () => {
  assert.notEqual(ASK_CARD_TONE.notice_action_required, ASK_CARD_TONE.question);
  assert.notEqual(ASK_CARD_TONE.notice_action_required, ASK_CARD_TONE.notice);
  assert.notEqual(ASK_DOT_TONE.notice_action_required, ASK_DOT_TONE.question);
  assert.notEqual(ASK_DOT_TONE.notice_action_required, ASK_DOT_TONE.notice);
  // The distinguishing tone must actually BE the codebase's established
  // "needs you, not just FYI" danger tone (see ProcessQuotaBar's degraded
  // tier), not just "some other string".
  assert.match(ASK_CARD_TONE.notice_action_required, /danger/);
  assert.match(ASK_DOT_TONE.notice_action_required, /danger/);
});

test("every NeedKind has exactly one card/dot tone entry — exhaustive, no silent fallback", () => {
  const kinds = ["question", "notice", "notice_action_required"].sort();
  assert.deepEqual(Object.keys(ASK_CARD_TONE).sort(), kinds);
  assert.deepEqual(Object.keys(ASK_DOT_TONE).sort(), kinds);
});

test("an ordinary self-clearing notice keeps the exact prior footer: no icon, the old copy key", () => {
  // Byte-for-byte the shape AskRow rendered unconditionally before this PR —
  // the stall hint, the stopped-worker hint, and an ordinary PR/MR update all
  // go through this one entry and must not regress.
  assert.deepEqual(NOTICE_FOOTER_VIEW.notice, {
    icon: null,
    textKey: "needs.selfClearing",
    className: "text-ink-faint",
  });
});

test("the give-up notice uses a DIFFERENT copy key than the self-clearing footer, plus an icon", () => {
  const actionRequired = NOTICE_FOOTER_VIEW.notice_action_required;
  assert.notEqual(actionRequired.textKey, NOTICE_FOOTER_VIEW.notice.textKey);
  assert.equal(actionRequired.textKey, "needs.actionRequired");
  assert.equal(actionRequired.icon, "alert");
  assert.notEqual(actionRequired.className, NOTICE_FOOTER_VIEW.notice.className);
});

test("every NoticeKind (every NeedKind except question) has exactly one footer view", () => {
  assert.deepEqual(Object.keys(NOTICE_FOOTER_VIEW).sort(), ["notice", "notice_action_required"]);
});
