import test from "node:test";
import assert from "node:assert/strict";
import {
  isProcessQuotaDegradedError,
  processQuotaNotice,
  shouldApplyProcessQuotaStatus,
} from "../../src/lib/processQuota.ts";
import type { ProcessQuotaLevel, ProcessQuotaStatus } from "../../src/lib/types.ts";

function snapshot(status: ProcessQuotaLevel, transitionSeq: number): ProcessQuotaStatus {
  return {
    status,
    processCount: 800,
    processLimit: 1_000,
    usagePercent: 80,
    warningPercent: 80,
    degradedPercent: 90,
    recoveryPercent: 70,
    transitionSeq,
  };
}

test("initial normal snapshot is quiet while elevated snapshots notify", () => {
  assert.equal(processQuotaNotice(null, snapshot("normal", 1)), null);
  assert.equal(processQuotaNotice(null, snapshot("warning", 1)), "warning");
  assert.equal(processQuotaNotice(null, snapshot("degraded", 1)), "degraded");
});

test("state transitions emit exactly one matching notice", () => {
  const normal = snapshot("normal", 1);
  const warning = snapshot("warning", 2);
  const degraded = snapshot("degraded", 3);
  const recovered = snapshot("normal", 4);

  assert.equal(processQuotaNotice(normal, warning), "warning");
  assert.equal(processQuotaNotice(warning, degraded), "degraded");
  assert.equal(processQuotaNotice(degraded, recovered), "recovered");
});

test("replayed and metric-only snapshots never repeat a notice", () => {
  const warning = snapshot("warning", 2);
  const sameTransition = { ...warning, processCount: 825, usagePercent: 82.5 };
  const laterMetricRefresh = { ...warning, transitionSeq: 3, processCount: 830 };

  assert.equal(processQuotaNotice(warning, sameTransition), null);
  assert.equal(processQuotaNotice(warning, laterMetricRefresh), null);
});

test("late snapshots cannot overwrite a newer transition", () => {
  const degraded = snapshot("degraded", 5);
  const staleWarning = snapshot("warning", 4);
  const sameStateRefresh = { ...degraded, processCount: 930, usagePercent: 93 };
  const inconsistentReplay = snapshot("normal", 5);

  assert.equal(shouldApplyProcessQuotaStatus(degraded, staleWarning), false);
  assert.equal(shouldApplyProcessQuotaStatus(degraded, sameStateRefresh), true);
  assert.equal(shouldApplyProcessQuotaStatus(degraded, inconsistentReplay), false);
});

test("the initial snapshot (null previous) is always applied", () => {
  assert.equal(shouldApplyProcessQuotaStatus(null, snapshot("normal", 0)), true);
  assert.equal(shouldApplyProcessQuotaStatus(null, snapshot("degraded", 3)), true);
});

test("degraded gate matcher accepts the stable code across supported error shapes", () => {
  assert.equal(isProcessQuotaDegradedError("process_quota_degraded"), true);
  assert.equal(isProcessQuotaDegradedError(new Error("process_quota_degraded: blocked")), true);
  assert.equal(isProcessQuotaDegradedError({ code: "process_quota_degraded" }), true);
  assert.equal(isProcessQuotaDegradedError("other_error"), false);
  assert.equal(isProcessQuotaDegradedError({ code: "other" }), false);
  assert.equal(isProcessQuotaDegradedError({}), false);
  assert.equal(isProcessQuotaDegradedError(null), false);
  assert.equal(isProcessQuotaDegradedError(undefined), false);
});
