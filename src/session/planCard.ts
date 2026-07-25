import type { LeadMessage } from "../lib/types";
import type { PlanCardSplitItem } from "./blocks/PlanCardBlock";

/** A plan_card message's sentinel payload, parsed. Mirrors the shape
 *  ChatTimeline's inline `plan_card` renderer reads off `m.content` — kept as
 *  its own small, independent parse (not imported from ChatTimeline, which is
 *  a virtualized-row component we deliberately don't wire into `useStore()`;
 *  see PR #72's lesson on bespoke async state over virtualized rows). */
export interface ParsedPlanCard {
  /** The plan_card row itself — its id is what `resolveActionCard` settles. */
  message: LeadMessage;
  title: string;
  requirements: string[];
  approach: string;
  risks: string[];
  split: PlanCardSplitItem[];
  /** Non-empty once approved (persisted via resolveActionCard's `name`); "" = still pending. */
  resolved: string;
}

function safeParseObj(content: string): Record<string, unknown> {
  try {
    const v: unknown = JSON.parse(content);
    return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : {};
  } catch {
    return {};
  }
}

function stringArray(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  return value.filter((item): item is string => typeof item === "string");
}

function isPlanSplitItem(value: unknown): value is PlanCardSplitItem {
  if (!value || typeof value !== "object") return false;
  const item = value as Record<string, unknown>;
  return (
    typeof item.name === "string" &&
    typeof item.repo === "string" &&
    (item.reason === undefined || typeof item.reason === "string")
  );
}

export function parsePlanCard(message: LeadMessage): ParsedPlanCard {
  const parsed = safeParseObj(message.content);
  return {
    message,
    title: typeof parsed.title === "string" ? parsed.title : "",
    requirements: stringArray(parsed.requirements),
    approach: typeof parsed.approach === "string" ? parsed.approach : "",
    risks: stringArray(parsed.risks),
    split: Array.isArray(parsed.split) ? parsed.split.filter(isPlanSplitItem) : [],
    resolved: typeof parsed.resolved === "string" ? parsed.resolved : "",
  };
}

/** The most recently emitted plan_card in a thread's message list — a re-plan
 *  supersedes an earlier one even though the older row stays in the timeline
 *  (rendered read-only there). `null` when the thread never got a plan_card,
 *  which is the normal case for trivial scope that skipped the plan step. */
export function latestPlanCard(all: LeadMessage[]): ParsedPlanCard | null {
  for (let i = all.length - 1; i >= 0; i--) {
    if (all[i].kind === "plan_card") return parsePlanCard(all[i]);
  }
  return null;
}
