import test from "node:test";
import assert from "node:assert/strict";
import { isInitialBottomSettled, isNearBottom } from "../../src/session/chatBottom.ts";

test("tracks user bottom intent from the real scroller metrics", () => {
  assert.equal(isNearBottom({
    scrollTop: 0,
    scrollHeight: 1200,
    clientHeight: 500,
  }), false);
  assert.equal(isNearBottom({
    scrollTop: 630,
    scrollHeight: 1200,
    clientHeight: 500,
  }), true);
});

test("does not reveal before the last virtual row is rendered", () => {
  assert.equal(isInitialBottomSettled({
    lastItemRendered: false,
    scrollTop: 400,
    scrollHeight: 900,
    clientHeight: 500,
  }), false);
});

test("does not reveal an intermediate height-correction frame", () => {
  assert.equal(isInitialBottomSettled({
    lastItemRendered: true,
    scrollTop: 400,
    scrollHeight: 1200,
    clientHeight: 500,
  }), false);
});

test("reveals once the rendered last row is at the DOM bottom", () => {
  assert.equal(isInitialBottomSettled({
    lastItemRendered: true,
    scrollTop: 700,
    scrollHeight: 1200,
    clientHeight: 500,
  }), true);
});
