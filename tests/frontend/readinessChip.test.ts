import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import ts from "typescript";

const require = createRequire(import.meta.url);
const chipPath = new URL("../../src/components/ReadinessChip.tsx", import.meta.url);
const readinessKeyPath = new URL("../../src/lib/readinessKey.ts", import.meta.url);

type ReadinessDto = {
  readiness: string;
  reasons: { code: string; direction_id: number | null }[];
};

type ReadinessFetchState =
  | { kind: "loading" }
  | { kind: "failed" }
  | { kind: "ready"; dto: ReadinessDto };

type ReadinessChipComponent = (props: {
  state: ReadinessFetchState;
}) => ReturnType<typeof createElement>;

let componentPromise: Promise<ReadinessChipComponent> | undefined;
type ReadinessKeyModule = {
  buildReadinessKey(input: {
    directions: { id: number; status: string }[];
    attentionIds: string[];
    worktrees: { directionId: number; exists: boolean }[];
    workerSessions: { directionId: number; sessionId: number; status: string }[];
    planStatus: string | null;
    prRevision: number;
  }): string;
  beginReadinessRefresh<T>(): ReadinessFetchState;
  completeReadinessRefresh<T>(dto: T): ReadinessFetchState;
  failReadinessRefresh<T>(): ReadinessFetchState;
  isDirectionUrgent(input: {
    readiness?: "review_ready" | "blocked" | "needs_you" | "unknown" | "failed";
    hasAttention: boolean;
    hasFailingCheck: boolean;
  }): boolean;
  selectVisibleReadiness<T>(
    stored: { threadId: number; key: string; state: ReadinessFetchState } | null,
    threadId: number | null,
    key: string,
  ): ReadinessFetchState;
  isReadinessResponseCurrent(requestedThreadId: number, currentThreadId: number | null): boolean;
  applyReadinessResponse<T>(
    current: ReadinessFetchState,
    request: { threadId: number; revision: number },
    currentThreadId: number | null,
    currentRevision: number,
    result: ReadinessFetchState,
  ): ReadinessFetchState;
};

let readinessKeyPromise: Promise<ReadinessKeyModule> | undefined;

function loadReadinessChip(): Promise<ReadinessChipComponent> {
  if (componentPromise) {
    return componentPromise;
  }
  componentPromise = (async () => {
    const source = readFileSync(chipPath, "utf8");
    let output = ts.transpileModule(source, {
      compilerOptions: {
        jsx: ts.JsxEmit.ReactJSX,
        module: ts.ModuleKind.ESNext,
        target: ts.ScriptTarget.ES2020,
      },
    }).outputText;
    for (const specifier of ["react/jsx-runtime", "react-i18next", "lucide-react"]) {
      const resolved = pathToFileURL(require.resolve(specifier)).href;
      output = output.replaceAll(`"${specifier}"`, `"${resolved}"`);
    }
    const encoded = Buffer.from(output).toString("base64");
    const module = await import(`data:text/javascript;base64,${encoded}`);
    return module.ReadinessChip as ReadinessChipComponent;
  })();
  return componentPromise;
}

function loadReadinessKey(): Promise<ReadinessKeyModule> {
  if (readinessKeyPromise) {
    return readinessKeyPromise;
  }
  readinessKeyPromise = (async () => {
    const source = readFileSync(readinessKeyPath, "utf8");
    const output = ts.transpileModule(source, {
      compilerOptions: {
        module: ts.ModuleKind.ESNext,
        target: ts.ScriptTarget.ES2020,
      },
    }).outputText;
    const encoded = Buffer.from(output).toString("base64");
    return (await import(`data:text/javascript;base64,${encoded}`)) as ReadinessKeyModule;
  })();
  return readinessKeyPromise;
}

function render(
  Component: ReadinessChipComponent,
  state: ReadinessFetchState,
): string {
  const previousWarn = console.warn;
  console.warn = (...args) => {
    if (String(args[0]).includes("react-i18next:: useTranslation")) {
      return;
    }
    previousWarn(...args);
  };
  try {
    return renderToStaticMarkup(createElement(Component, { state }));
  } finally {
    console.warn = previousWarn;
  }
}

test("ReadinessChip renders all five backend discriminators", async () => {
  const Component = await loadReadinessChip();
  const states = [
    "review_ready",
    "blocked",
    "needs_you",
    "unknown",
    "failed",
  ];

  for (const readiness of states) {
    const html = render(Component, { kind: "ready", dto: { readiness, reasons: [] } });
    assert.match(html, new RegExp(`readiness\\.status\\.${readiness}`));
  }
});

test("ReadinessChip renders a pending refresh as unknown with missing evidence", async () => {
  const Component = await loadReadinessChip();
  const html = render(Component, { kind: "loading" });
  assert.match(html, /readiness\.status\.unknown/);
  assert.match(html, /readiness\.reason\.evidence_missing/);
});

test("a rejected readiness fetch renders unavailable rather than loading", async () => {
  const { failReadinessRefresh } = await loadReadinessKey();
  const Component = await loadReadinessChip();
  const rejected = failReadinessRefresh();
  assert.equal(rejected.kind, "failed");

  const html = render(Component, rejected);
  assert.match(html, /readiness\.status\.unavailable/);
  assert.match(html, /<svg/);
  assert.doesNotMatch(html, /readiness\.reason\.evidence_missing/);
});

test("ReadinessChip renders every readiness reason", async () => {
  const Component = await loadReadinessChip();
  const codes = [
    "no_active_lanes",
    "upstream_unmet",
    "evidence_missing",
    "remote_unknown",
    "execution_drifted",
    "policy_gate_pending",
    "open_need",
    "checks_failing",
    "checks_unknown",
    "worker_failed",
    "in_progress",
    "pr_ci_pending",
    "pr_ci_failing",
    "pr_review_changes_requested",
    "pr_threads_unresolved",
    "pr_conflict",
    "pr_closed_unmerged",
  ];
  const reasons = codes.map((code) => ({ code, direction_id: 171 }));
  const html = render(Component, { kind: "ready", dto: { readiness: "blocked", reasons } });

  for (const code of codes) {
    assert.match(html, new RegExp(`readiness\\.reason\\.${code}`));
  }
});

test("readiness key includes direction statuses, attention identities, worktrees, latest workers, plan, and PR revision", async () => {
  const { buildReadinessKey } = await loadReadinessKey();
  const base = buildReadinessKey({
    directions: [
      { id: 9, status: "review" },
      { id: 4, status: "working" },
    ],
    attentionIds: ["need-9", "need-4"],
    worktrees: [
      { directionId: 9, exists: true },
      { directionId: 4, exists: true },
    ],
    workerSessions: [
      { directionId: 9, sessionId: 12, status: "idle" },
      { directionId: 4, sessionId: 8, status: "running" },
    ],
    planStatus: "proposed",
    prRevision: 0,
  });
  assert.equal(
    base,
    "directions:4:working,9:review|attention:need-4,need-9|worktrees:4:true,9:true|workers:4:8:running,9:12:idle|plan:proposed|pr:0",
  );
  assert.notEqual(
    base,
    buildReadinessKey({
      directions: [
        { id: 9, status: "review" },
        { id: 4, status: "done" },
      ],
      attentionIds: ["need-9", "need-4"],
      worktrees: [
        { directionId: 9, exists: true },
        { directionId: 4, exists: true },
      ],
      workerSessions: [
        { directionId: 9, sessionId: 12, status: "idle" },
        { directionId: 4, sessionId: 8, status: "running" },
      ],
      planStatus: "proposed",
      prRevision: 0,
    }),
  );
  assert.notEqual(
    base,
    buildReadinessKey({
      directions: [
        { id: 9, status: "review" },
        { id: 4, status: "working" },
      ],
      attentionIds: ["need-9", "need-8"],
      worktrees: [
        { directionId: 9, exists: true },
        { directionId: 4, exists: true },
      ],
      workerSessions: [
        { directionId: 9, sessionId: 12, status: "idle" },
        { directionId: 4, sessionId: 8, status: "running" },
      ],
      planStatus: "proposed",
      prRevision: 0,
    }),
  );
  assert.notEqual(
    base,
    buildReadinessKey({
      directions: [
        { id: 9, status: "review" },
        { id: 4, status: "working" },
      ],
      attentionIds: ["need-9", "need-4"],
      worktrees: [
        { directionId: 9, exists: true },
        { directionId: 4, exists: true },
      ],
      workerSessions: [
        { directionId: 9, sessionId: 12, status: "idle" },
        { directionId: 4, sessionId: 8, status: "running" },
      ],
      planStatus: "confirmed",
      prRevision: 0,
    }),
  );
  assert.notEqual(
    base,
    buildReadinessKey({
      directions: [
        { id: 9, status: "review" },
        { id: 4, status: "working" },
      ],
      attentionIds: ["need-9", "need-4"],
      worktrees: [
        { directionId: 9, exists: true },
        { directionId: 4, exists: true },
      ],
      workerSessions: [
        { directionId: 9, sessionId: 12, status: "idle" },
        { directionId: 4, sessionId: 8, status: "running" },
      ],
      planStatus: "proposed",
      prRevision: 1,
    }),
  );
});

test("worktree removal synchronously hides an otherwise ready verdict", async () => {
  const { buildReadinessKey, selectVisibleReadiness } = await loadReadinessKey();
  const liveKey = buildReadinessKey({
    directions: [{ id: 17, status: "review" }],
    attentionIds: [],
    worktrees: [{ directionId: 17, exists: true }],
    workerSessions: [],
    planStatus: null,
    prRevision: 0,
  });
  const removedKey = buildReadinessKey({
    directions: [{ id: 17, status: "review" }],
    attentionIds: [],
    worktrees: [{ directionId: 17, exists: false }],
    workerSessions: [],
    planStatus: null,
    prRevision: 0,
  });
  const dto = { readiness: "review_ready", reasons: [] };
  const ready = { kind: "ready" as const, dto };
  const stored = { threadId: 17, key: liveKey, state: ready };

  assert.notEqual(liveKey, removedKey, "a worktree existence flip changes the request key");
  assert.equal(selectVisibleReadiness(stored, 17, liveKey), ready);
  assert.deepEqual(
    selectVisibleReadiness(stored, 17, removedKey),
    { kind: "loading" },
    "the prior review-ready result is hidden before the refresh effect runs",
  );
});

test("plan status change synchronously hides an otherwise ready verdict", async () => {
  const { buildReadinessKey, selectVisibleReadiness } = await loadReadinessKey();
  const noPlanKey = buildReadinessKey({
    directions: [{ id: 17, status: "review" }],
    attentionIds: [],
    worktrees: [{ directionId: 17, exists: true }],
    workerSessions: [],
    planStatus: null,
    prRevision: 0,
  });
  const proposedPlanKey = buildReadinessKey({
    directions: [{ id: 17, status: "review" }],
    attentionIds: [],
    worktrees: [{ directionId: 17, exists: true }],
    workerSessions: [],
    planStatus: "proposed",
    prRevision: 0,
  });
  const dto = { readiness: "review_ready", reasons: [] };
  const ready = { kind: "ready" as const, dto };
  const stored = { threadId: 17, key: noPlanKey, state: ready };

  assert.notEqual(noPlanKey, proposedPlanKey, "a proposed plan changes the request key");
  assert.equal(selectVisibleReadiness(stored, 17, noPlanKey), ready);
  assert.deepEqual(
    selectVisibleReadiness(stored, 17, proposedPlanKey),
    { kind: "loading" },
    "the prior review-ready result is hidden before the refresh effect runs",
  );
});

test("worker terminal status changes synchronously hide a prior verdict", async () => {
  const { buildReadinessKey, selectVisibleReadiness } = await loadReadinessKey();
  const runningKey = buildReadinessKey({
    directions: [{ id: 17, status: "working" }],
    attentionIds: [],
    worktrees: [{ directionId: 17, exists: true }],
    workerSessions: [
      { directionId: 17, sessionId: 2, status: "idle" },
      { directionId: 17, sessionId: 3, status: "running" },
    ],
    planStatus: null,
    prRevision: 0,
  });
  const failedKey = buildReadinessKey({
    directions: [{ id: 17, status: "working" }],
    attentionIds: [],
    worktrees: [{ directionId: 17, exists: true }],
    workerSessions: [
      { directionId: 17, sessionId: 2, status: "idle" },
      { directionId: 17, sessionId: 3, status: "error" },
    ],
    planStatus: null,
    prRevision: 0,
  });
  const ready = { kind: "ready" as const, dto: { readiness: "review_ready", reasons: [] } };
  const stored = { threadId: 17, key: runningKey, state: ready };

  assert.notEqual(runningKey, failedKey, "the latest worker terminal state changes the key");
  assert.equal(selectVisibleReadiness(stored, 17, runningKey), ready);
  assert.deepEqual(selectVisibleReadiness(stored, 17, failedKey), { kind: "loading" });
});

test("board urgency preserves baseline signals while consuming lane verdicts", async () => {
  const { isDirectionUrgent } = await loadReadinessKey();

  assert.equal(
    isDirectionUrgent({ readiness: undefined, hasAttention: true, hasFailingCheck: false }),
    true,
    "attention remains urgent while a verdict is unavailable",
  );
  assert.equal(
    isDirectionUrgent({ readiness: undefined, hasAttention: false, hasFailingCheck: true }),
    true,
    "an existing failing check remains urgent while a verdict is unavailable",
  );
  assert.equal(
    isDirectionUrgent({ readiness: "blocked", hasAttention: false, hasFailingCheck: false }),
    true,
    "a blocked backend lane is urgent",
  );
  assert.equal(
    isDirectionUrgent({ readiness: "review_ready", hasAttention: false, hasFailingCheck: false }),
    false,
    "a clear lane with no legacy signal is not urgent",
  );
});

test("visible readiness is synchronously invalidated by key or thread changes", async () => {
  const { selectVisibleReadiness } = await loadReadinessKey();
  const ready = { kind: "ready" as const, dto: { readiness: "blocked", reasons: [] } };
  const stored = { threadId: 17, key: "directions:17:review", state: ready };

  assert.equal(
    selectVisibleReadiness(stored, 17, "directions:17:review"),
    ready,
    "the DTO remains visible for the source thread and key",
  );
  assert.deepEqual(
    selectVisibleReadiness(stored, 17, "directions:17:done"),
    { kind: "loading" },
    "a changed refresh key hides old evidence before an effect runs",
  );
  assert.deepEqual(
    selectVisibleReadiness(stored, 18, "directions:17:review"),
    { kind: "loading" },
    "a thread switch hides a prior thread's DTO",
  );
});

test("readiness response applies only to its current thread", async () => {
  const { isReadinessResponseCurrent } = await loadReadinessKey();
  assert.equal(isReadinessResponseCurrent(17, 17), true);
  assert.equal(isReadinessResponseCurrent(17, 18), false);
  assert.equal(isReadinessResponseCurrent(17, null), false);
});

test("readiness refresh clears old evidence until only its live response applies", async () => {
  const { applyReadinessResponse, beginReadinessRefresh, completeReadinessRefresh } =
    await loadReadinessKey();
  const pending = beginReadinessRefresh();
  assert.deepEqual(pending, { kind: "loading" }, "a refresh starts at unknown/evidence-missing");
  assert.deepEqual(pending, { kind: "loading" }, "a delayed response leaves the pending state unknown");

  const staleThread = applyReadinessResponse(
    pending,
    { threadId: 17, revision: 1 },
    18,
    1,
    completeReadinessRefresh({ readiness: "review_ready", reasons: [] }),
  );
  assert.deepEqual(staleThread, { kind: "loading" }, "a stale thread response cannot write evidence");

  const staleRevision = applyReadinessResponse(
    pending,
    { threadId: 17, revision: 1 },
    17,
    2,
    completeReadinessRefresh({ readiness: "review_ready", reasons: [] }),
  );
  assert.deepEqual(staleRevision, { kind: "loading" }, "an older same-thread response cannot write evidence");

  const applied = applyReadinessResponse(
    pending,
    { threadId: 17, revision: 2 },
    17,
    2,
    completeReadinessRefresh({ readiness: "review_ready", reasons: [] }),
  );
  assert.deepEqual(applied, {
    kind: "ready",
    dto: { readiness: "review_ready", reasons: [] },
  });
});
