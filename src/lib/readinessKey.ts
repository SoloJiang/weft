import type { LaneReadiness } from "./types";

export interface ReadinessDirectionSignature {
  id: number;
  status: string;
}

export interface ReadinessWorktreeSignature {
  directionId: number;
  exists: boolean;
}

export interface ReadinessKeyInput {
  directions: ReadinessDirectionSignature[];
  attentionIds: string[];
  worktrees: ReadinessWorktreeSignature[];
  planStatus: string | null;
  prRevision: number;
}

export interface ReadinessRequest {
  threadId: number;
  revision: number;
}

/** A response and the exact refresh input that produced it. */
export interface StoredReadiness<T> {
  threadId: number;
  key: string;
  dto: T | null;
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

/** Stable refresh input for one backend readiness read. */
export function buildReadinessKey(input: ReadinessKeyInput): string {
  const directions = [...input.directions]
    .sort((left, right) => left.id - right.id)
    .map((direction) => `${direction.id}:${direction.status}`)
    .join(",");
  const attentionIds = [...input.attentionIds].sort().join(",");
  const worktrees = [...input.worktrees]
    .sort((left, right) => left.directionId - right.directionId)
    .map((worktree) => `${worktree.directionId}:${worktree.exists}`)
    .join(",");
  const planStatus = input.planStatus ?? "";
  return [
    `directions:${directions}`,
    `attention:${attentionIds}`,
    `worktrees:${worktrees}`,
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
): T | null {
  if (!stored || stored.threadId !== threadId) {
    return null;
  }
  if (stored.key !== key) {
    return null;
  }
  return stored.dto;
}

/** A refresh never presents a prior verdict as current evidence. */
export function beginReadinessRefresh(): null {
  return null;
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
  current: T | null,
  request: ReadinessRequest,
  currentThreadId: number | null,
  currentRevision: number,
  result: T | null,
): T | null {
  if (!isReadinessResponseApplicable(request, currentThreadId, currentRevision)) {
    return current;
  }
  return result;
}
