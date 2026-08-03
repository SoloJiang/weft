import test from "node:test";
import assert from "node:assert/strict";
import { selectThreadActivity, type ThreadActivitySession } from "../../src/state/threadActivity.ts";
import { threadActivityMarkers } from "../../src/components/ui/threadActivityMarkers.ts";

function session(directionId: number, status: ThreadActivitySession["status"]): ThreadActivitySession {
  return { directionId, status };
}

test("selectThreadActivity: idle ignores idle/exited and unrelated worker sessions", () => {
  assert.deepEqual(
    selectThreadActivity({
      workerSessions: [session(1, "idle"), session(1, "exited"), session(2, "running")],
      directionIds: [1],
      leadState: "idle",
    }),
    { kind: "idle", running: 0 },
  );
});

test("selectThreadActivity: running counts busy lead and running workers", () => {
  assert.deepEqual(
    selectThreadActivity({
      workerSessions: [session(1, "running"), session(1, "running"), session(1, "idle")],
      directionIds: [1],
      leadState: "busy",
    }),
    { kind: "running", running: 3 },
  );
});

test("threadActivityMarkers: idle has no compact marker", () => {
  assert.deepEqual(threadActivityMarkers({ kind: "idle", running: 0 }), []);
});

test("threadActivityMarkers: running has one pulsing marker count", () => {
  assert.deepEqual(
    threadActivityMarkers({ kind: "running", running: 2 }),
    [{ kind: "running", count: 2 }],
  );
});
