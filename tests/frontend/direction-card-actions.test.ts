import assert from "node:assert/strict";
import test from "node:test";
import {
  canComplete,
  canContinueDirection,
  directionCardPrimary,
} from "../../src/lib/direction-card-actions.ts";

test("Needs You is the primary action even on review", () => {
  assert.equal(directionCardPrimary(true, "review"), "handle");
  assert.equal(directionCardPrimary(false, "review"), "viewChanges");
  assert.equal(directionCardPrimary(false, "working"), "openSession");
  assert.equal(directionCardPrimary(false, "done"), "openSession");
});

test("can_complete matches the scheduler: only review", () => {
  assert.equal(canComplete("review"), true);
  assert.equal(canComplete("working"), false);
  assert.equal(canComplete("planning"), false);
  assert.equal(canComplete("done"), false);
  assert.equal(canComplete("queued"), false);
});

test("Continue is offered on review and done", () => {
  assert.equal(canContinueDirection("review"), true);
  assert.equal(canContinueDirection("done"), true);
  assert.equal(canContinueDirection("working"), false);
  assert.equal(canContinueDirection("planning"), false);
});
