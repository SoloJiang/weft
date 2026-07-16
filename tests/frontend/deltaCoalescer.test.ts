import test from "node:test";
import assert from "node:assert/strict";
import { createDeltaCoalescer, type DeltaBatch } from "../../src/state/deltaCoalescer.ts";

/** Manual scheduler: flushes fire only when the test says so. */
function harness() {
  const flushes: DeltaBatch[] = [];
  let tick: (() => void) | null = null;
  let scheduled = 0;
  let cancelled = 0;
  const coalescer = createDeltaCoalescer(
    (batch) => flushes.push(batch),
    (flush) => {
      scheduled += 1;
      tick = flush;
      return () => {
        cancelled += 1;
        tick = null;
      };
    },
  );
  return {
    coalescer,
    flushes,
    fire: () => {
      const t = tick;
      tick = null;
      t?.();
    },
    counts: () => ({ scheduled, cancelled }),
  };
}

test("batches many pushes into one flush per scheduled tick", () => {
  const h = harness();
  h.coalescer.push(1, 10, "hel");
  h.coalescer.push(1, 10, "lo ");
  h.coalescer.push(1, 10, "world");
  assert.equal(h.counts().scheduled, 1);
  assert.equal(h.flushes.length, 0);
  h.fire();
  assert.deepEqual(h.flushes, [[{ threadId: 1, messageId: 10, text: "hello world" }]]);
});

test("keeps per-message concatenation order and cross-message arrival order", () => {
  const h = harness();
  h.coalescer.push(1, 10, "a1");
  h.coalescer.push(2, 20, "b1");
  h.coalescer.push(1, 10, "a2");
  h.fire();
  assert.deepEqual(h.flushes, [
    [
      { threadId: 1, messageId: 10, text: "a1a2" },
      { threadId: 2, messageId: 20, text: "b1" },
    ],
  ]);
});

test("flushNow drains synchronously and cancels the scheduled tick", () => {
  const h = harness();
  h.coalescer.push(1, 10, "x");
  h.coalescer.flushNow();
  assert.deepEqual(h.flushes, [[{ threadId: 1, messageId: 10, text: "x" }]]);
  assert.equal(h.counts().cancelled, 1);
  h.fire(); // a late tick must not double-flush
  assert.equal(h.flushes.length, 1);
});

test("flushNow with nothing pending is a no-op", () => {
  const h = harness();
  h.coalescer.flushNow();
  assert.equal(h.flushes.length, 0);
});

test("pushes after a flush schedule a fresh tick", () => {
  const h = harness();
  h.coalescer.push(1, 10, "one");
  h.fire();
  h.coalescer.push(1, 10, " two");
  assert.equal(h.counts().scheduled, 2);
  h.fire();
  assert.deepEqual(h.flushes, [
    [{ threadId: 1, messageId: 10, text: "one" }],
    [{ threadId: 1, messageId: 10, text: " two" }],
  ]);
});

test("survives a scheduler that fires synchronously", () => {
  const flushes: DeltaBatch[] = [];
  const coalescer = createDeltaCoalescer(
    (batch) => flushes.push(batch),
    (flush) => {
      flush();
      return () => {};
    },
  );
  coalescer.push(1, 10, "a");
  coalescer.push(1, 10, "b");
  assert.deepEqual(flushes, [
    [{ threadId: 1, messageId: 10, text: "a" }],
    [{ threadId: 1, messageId: 10, text: "b" }],
  ]);
});
