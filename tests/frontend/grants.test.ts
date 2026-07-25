import test from "node:test";
import assert from "node:assert/strict";
import { readOnlyRevokeDir, readOnlyScopeOf } from "../../src/lib/grants.ts";
import type { ReadOnlyGrants } from "../../src/lib/types.ts";

// issue #103: read-only auto-allow scope derivation. These are pure functions
// that decide what the UI SHOWS and what it REVOKES — a wrong answer here
// either hides a live grant from the human (can't revoke what you can't see)
// or points a revoke at the wrong scope (clicking "revoke" on what looks like
// a session grant that's actually covered by the issue-wide one, leaving the
// human thinking they revoked something they didn't).

function grants(issue: number[], session: { thread: number; dir: string }[]): ReadOnlyGrants {
  return { issue, session };
}

test("readOnlyScopeOf: no grants at all is none", () => {
  assert.equal(readOnlyScopeOf(grants([], []), 1, "10"), "none");
});

test("readOnlyScopeOf: an issue-wide grant for this thread is issue, regardless of dir", () => {
  assert.equal(readOnlyScopeOf(grants([1], []), 1, "10"), "issue");
  assert.equal(readOnlyScopeOf(grants([1], []), 1, "anything-else"), "issue");
});

test("readOnlyScopeOf: a session grant matching BOTH thread and dir is session", () => {
  assert.equal(readOnlyScopeOf(grants([], [{ thread: 1, dir: "10" }]), 1, "10"), "session");
});

test("readOnlyScopeOf: a session grant for a different dir on the same thread does not match", () => {
  assert.equal(readOnlyScopeOf(grants([], [{ thread: 1, dir: "10" }]), 1, "11"), "none");
});

test("readOnlyScopeOf: a session grant for a different thread with the same dir string does not match", () => {
  assert.equal(readOnlyScopeOf(grants([], [{ thread: 2, dir: "10" }]), 1, "10"), "none");
});

test("readOnlyScopeOf: issue wins over session when both are set for the same thread", () => {
  // The issue-wide grant already covers every dir; revoking "session" alone
  // would be a no-op while it's still active, so the UI must report the
  // grant that's actually doing the work.
  const g = grants([1], [{ thread: 1, dir: "10" }]);
  assert.equal(readOnlyScopeOf(g, 1, "10"), "issue");
});

test("readOnlyRevokeDir: issue scope revokes the whole issue (dir=null), regardless of the passed dir", () => {
  assert.equal(readOnlyRevokeDir("issue", "10"), null);
  assert.equal(readOnlyRevokeDir("issue", ""), null);
});

test("readOnlyRevokeDir: session scope revokes just that session (dir passed through)", () => {
  assert.equal(readOnlyRevokeDir("session", "10"), "10");
  assert.equal(readOnlyRevokeDir("session", ""), "");
});

test("readOnlyRevokeDir: none scope has nothing to revoke but still returns the dir, not null", () => {
  // Callers gate the revoke button on scope !== "none" before ever calling
  // this — asserted here so that invariant stays visible if it's ever relaxed.
  assert.equal(readOnlyRevokeDir("none", "10"), "10");
});
