import test from "node:test";
import assert from "node:assert/strict";
import { resetParts } from "../../src/settings/engineQuotaFormat.ts";

const NOW_MS = 1_700_000_000_000; // fixed instant, seconds-aligned
const NOW_SEC = Math.floor(NOW_MS / 1000);

test("resetParts: null when the reset time already passed", () => {
  assert.equal(resetParts(NOW_SEC - 1, NOW_MS), null);
  assert.equal(resetParts(NOW_SEC, NOW_MS), null);
});

test("resetParts: minutes-only granularity under an hour away", () => {
  const parts = resetParts(NOW_SEC + 25 * 60, NOW_MS);
  assert.deepEqual(parts, { granularity: "minutes", days: 0, hours: 0, minutes: 25 });
});

test("resetParts: hours granularity under a day away", () => {
  const parts = resetParts(NOW_SEC + 3 * 3600 + 15 * 60, NOW_MS);
  assert.deepEqual(parts, { granularity: "hours", days: 0, hours: 3, minutes: 15 });
});

test("resetParts: days granularity a day or more away", () => {
  const parts = resetParts(NOW_SEC + 2 * 86400 + 5 * 3600, NOW_MS);
  assert.deepEqual(parts, { granularity: "days", days: 2, hours: 5, minutes: 0 });
});

test("resetParts: exactly on an hour/day boundary rounds down cleanly", () => {
  assert.deepEqual(resetParts(NOW_SEC + 3600, NOW_MS), {
    granularity: "hours",
    days: 0,
    hours: 1,
    minutes: 0,
  });
  assert.deepEqual(resetParts(NOW_SEC + 86400, NOW_MS), {
    granularity: "days",
    days: 1,
    hours: 0,
    minutes: 0,
  });
});
