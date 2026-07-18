import test from "node:test";
import assert from "node:assert/strict";
import {
  beginChatHistoryLoad,
  failChatHistoryLoad,
  workerChatHistoryStatus,
} from "../../src/state/chatHistory.ts";

test("an initial history load blocks until it is ready", () => {
  assert.equal(beginChatHistoryLoad(undefined), "loading");
  assert.equal(beginChatHistoryLoad("error"), "loading");
});

test("a refresh keeps already-loaded history visible", () => {
  assert.equal(beginChatHistoryLoad("ready"), "ready");
  assert.equal(failChatHistoryLoad("ready"), "ready");
});

test("an initial history failure becomes an explicit error", () => {
  assert.equal(failChatHistoryLoad(undefined), "error");
  assert.equal(failChatHistoryLoad("loading"), "error");
});

test("a worker without a session is empty only after its slot lookup completes", () => {
  assert.equal(workerChatHistoryStatus(null, "loading", "ready"), "loading");
  assert.equal(workerChatHistoryStatus(null, "error", "ready"), "error");
  assert.equal(workerChatHistoryStatus(null, "ready", "loading"), "ready");
});

test("a worker session waits for its parent thread history", () => {
  assert.equal(workerChatHistoryStatus(42, "ready", undefined), "loading");
  assert.equal(workerChatHistoryStatus(42, "ready", "error"), "error");
  assert.equal(workerChatHistoryStatus(42, "loading", "ready"), "ready");
});
