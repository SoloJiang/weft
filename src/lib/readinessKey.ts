import type { LaneReadiness } from "./types";

export interface ReadinessDirectionSignature {
  id: number;
  status: string;
}

export interface ReadinessWorktreeSignature {
  directionId: number;
  /** Stable row identity; avoids placing a local filesystem path in the key. */
  worktreeId?: number;
  exists: boolean;
}

export interface ReadinessWorktreeRow {
  id: number;
  exists: boolean;
}

/** A live worker session whose terminal transition can invalidate readiness. */
export interface ReadinessWorkerSessionSignature {
  directionId: number;
  sessionId: number;
  status: string;
}

export interface ReadinessKeyInput {
  directions: ReadinessDirectionSignature[];
  attentionIds: string[];
  worktrees: ReadinessWorktreeSignature[];
  workerSessions: ReadinessWorkerSessionSignature[];
  planStatus: string | null;
  prRevision: number;
}

export interface ReadinessRequest {
  threadId: number;
  revision: number;
}

/** The client-visible lifecycle of one readiness read. */
export type ReadinessFetchState<T> =
  | { kind: "loading" }
  | { kind: "failed" }
  | { kind: "ready"; dto: T };

/** A response state and the exact refresh input that produced it. */
export interface StoredReadiness<T> {
  threadId: number;
  key: string;
  state: ReadinessFetchState<T>;
}

export interface DirectionUrgencyInput {
  readiness: LaneReadiness | undefined;
  hasAttention: boolean;
  hasFailingCheck: boolean;
}

const URGENT_LANE_READINESS: Record<LaneReadiness, boolean> = {
  review_ready: false,
  blocked: true,
  needs_you: true,
  unknown: false,
  failed: true,
};

function compareReadinessWorktrees(
  left: ReadinessWorktreeSignature,
  right: ReadinessWorktreeSignature,
): number {
  const directionOrder = left.directionId - right.directionId;
  if (directionOrder !== 0) {
    return directionOrder;
  }
  const leftWorktreeId = left.worktreeId ?? left.directionId;
  const rightWorktreeId = right.worktreeId ?? right.directionId;
  const worktreeOrder = leftWorktreeId - rightWorktreeId;
  if (worktreeOrder !== 0) {
    return worktreeOrder;
  }
  return Number(left.exists) - Number(right.exists);
}

function readinessWorktreeIdentity(worktree: ReadinessWorktreeSignature): string {
  if (worktree.worktreeId == null) {
    return String(worktree.directionId);
  }
  return `${worktree.directionId}:${worktree.worktreeId}`;
}

/** Preserve every registered worktree row while omitting irrelevant metadata. */
export function buildReadinessWorktreeSignatures(
  directions: ReadinessDirectionSignature[],
  worktreesByDirection: Record<number, ReadinessWorktreeRow[]>,
): ReadinessWorktreeSignature[] {
  return directions.flatMap((direction) =>
    (worktreesByDirection[direction.id] ?? []).map((worktree) => ({
      directionId: direction.id,
      worktreeId: worktree.id,
      exists: worktree.exists,
    })),
  );
}

/** The newest known session per relevant direction, in deterministic order. */
export function latestWorkerSessionSignatures(
  directionIds: number[],
  sessions: ReadinessWorkerSessionSignature[],
): ReadinessWorkerSessionSignature[] {
  const relevantDirections = new Set(directionIds);
  const latestByDirection = new Map<number, ReadinessWorkerSessionSignature>();
  for (const session of sessions) {
    if (!relevantDirections.has(session.directionId)) {
      continue;
    }
    const current = latestByDirection.get(session.directionId);
    if (!current || session.sessionId > current.sessionId) {
      latestByDirection.set(session.directionId, session);
    }
  }
  return [...latestByDirection.values()].sort(
    (left, right) => left.directionId - right.directionId,
  );
}

/** Stable refresh input for one backend readiness read. */
export function buildReadinessKey(input: ReadinessKeyInput): string {
  const directions = [...input.directions]
    .sort((left, right) => left.id - right.id)
    .map((direction) => `${direction.id}:${direction.status}`)
    .join(",");
  const attentionIds = [...input.attentionIds].sort().join(",");
  const worktrees = [...input.worktrees]
    .sort(compareReadinessWorktrees)
    .map((worktree) => `${readinessWorktreeIdentity(worktree)}:${worktree.exists}`)
    .join(",");
  const workerSessions = latestWorkerSessionSignatures(
    input.directions.map((direction) => direction.id),
    input.workerSessions,
  )
    .map((session) => `${session.directionId}:${session.sessionId}:${session.status}`)
    .join(",");
  const planStatus = input.planStatus ?? "";
  return [
    `directions:${directions}`,
    `attention:${attentionIds}`,
    `worktrees:${worktrees}`,
    `workers:${workerSessions}`,
    `plan:${planStatus}`,
    `pr:${input.prRevision}`,
  ].join("|");
}

/**
 * Board urgency is a sorting affinity, not a second delivery-readiness
 * calculation. It preserves the pre-readiness attention and already-recorded
 * check-failure signals while consuming the backend lane verdict directly.
 */
export function isDirectionUrgent(input: DirectionUrgencyInput): boolean {
  if (input.readiness && URGENT_LANE_READINESS[input.readiness]) {
    return true;
  }
  if (input.hasAttention) {
    return true;
  }
  return input.hasFailingCheck;
}

/** Never render a response as current after its thread or refresh key changed. */
export function selectVisibleReadiness<T>(
  stored: StoredReadiness<T> | null,
  threadId: number | null,
  key: string,
): ReadinessFetchState<T> {
  if (!stored || stored.threadId !== threadId) {
    return beginReadinessRefresh();
  }
  if (stored.key !== key) {
    return beginReadinessRefresh();
  }
  return stored.state;
}

/** A refresh never presents a prior verdict as current evidence. */
export function beginReadinessRefresh<T>(): ReadinessFetchState<T> {
  return { kind: "loading" };
}

/** A successful response becomes the only current delivery evidence. */
export function completeReadinessRefresh<T>(dto: T): ReadinessFetchState<T> {
  return { kind: "ready", dto };
}

/** A rejected request is unavailable until the next refresh starts. */
export function failReadinessRefresh<T>(): ReadinessFetchState<T> {
  return { kind: "failed" };
}

/** A response belongs only to the thread that was current when it was requested. */
export function isReadinessResponseCurrent(
  requestedThreadId: number,
  currentThreadId: number | null,
): boolean {
  return requestedThreadId === currentThreadId;
}

/** The thread identity and local request sequence must both still match. */
export function isReadinessResponseApplicable(
  request: ReadinessRequest,
  currentThreadId: number | null,
  currentRevision: number,
): boolean {
  if (!isReadinessResponseCurrent(request.threadId, currentThreadId)) {
    return false;
  }
  return request.revision === currentRevision;
}

/** Apply a fetch result only when the refresh that produced it is still live. */
export function applyReadinessResponse<T>(
  current: ReadinessFetchState<T>,
  request: ReadinessRequest,
  currentThreadId: number | null,
  currentRevision: number,
  result: ReadinessFetchState<T>,
): ReadinessFetchState<T> {
  if (!isReadinessResponseApplicable(request, currentThreadId, currentRevision)) {
    return current;
  }
  return result;
}
