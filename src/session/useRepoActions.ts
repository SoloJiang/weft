// Shared hook for the three repo-onboarding flows (add / new / clone).
// The hook owns workspace resolution, dialog/picker orchestration, toasts,
// and best-effort回灌 into the lead thread; the caller supplies the text
// prompt UI (Modal/inline form) via `promptText` so we keep zero JSX here.

import { useCallback, useState } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { useTranslation } from "react-i18next";

import { toast } from "../components/Toast";
import { currentLang } from "../i18n";
import { useStore } from "../state/store";
import { api } from "../lib/api";
import type { RepoActionCommandResult } from "../lib/api";
import { repoNameFromUrl } from "../lib/gitUrl";

export type RepoActionKind = "add" | "new" | "clone";

export interface RepoActionContext {
  /** When present, the result is posted back to the lead thread. */
  threadId?: number;
  /** The action_card message id, so a successful flow collapses that card to a
   *  settled summary (closes the loop; prevents a re-click double-add). */
  messageId?: number;
  /** Override the active workspace (e.g., when invoked from a card pinned
   *  to a specific workspace). Falls back to the active workspace. */
  preferredWorkspaceId?: number | null;
}

export interface RepoActionInvocation {
  actionId: string;
  kind: RepoActionKind;
  ctx: RepoActionContext;
  /** Caller-supplied text-input prompt. Returns user input or null on cancel. */
  promptText: (title: string, placeholder?: string) => Promise<string | null>;
}

type RepoActionResult =
  | {
      status: "ok";
      execution_outcome: "freshly_completed" | "replayed";
      repo_id: string;
      name: string;
      local_git_path: string;
    }
  | { status: "in_progress" }
  | { status: "error"; message: string }
  | { status: "stale"; message: string }
  | { status: "cancelled" };

type Translate = (key: string, opts?: Record<string, unknown>) => string;

export function useRepoActions() {
  const { t } = useTranslation();
  const { activeWorkspaceId, refreshReposAndMap, refreshNeeds } = useStore();
  const [busy, setBusy] = useState<Record<string, boolean>>({});

  const setBusyFor = useCallback((id: string, v: boolean) => {
    setBusy((b) => ({ ...b, [id]: v }));
  }, []);

  const resolveWorkspaceId = useCallback(
    async (ctx: RepoActionContext): Promise<number | null> => {
      if (ctx.preferredWorkspaceId != null) return ctx.preferredWorkspaceId;
      if (activeWorkspaceId != null) return activeWorkspaceId;
      toast(t("repoActions.noWorkspaceToast"));
      return null;
    },
    [activeWorkspaceId, t],
  );

  const maybePost = useCallback(
    async (inv: RepoActionInvocation, payload: Record<string, unknown>) => {
      if (inv.ctx.threadId == null) return;
      const full = {
        tool: "repo_action",
        action_id: inv.actionId,
        kind: inv.kind,
        ...payload,
      };
      try {
        await api.postLeadToolResult(inv.ctx.threadId, full, currentLang());
      } catch (e) {
        console.warn("[weft] lead repo-action feedback delivery failed", e);
      }
    },
    [],
  );

  const run = useCallback(
    async (inv: RepoActionInvocation) => {
      setBusyFor(inv.actionId, true);
      try {
        const guarded = inv.ctx.threadId != null && inv.ctx.messageId != null;
        const wsId = await resolveWorkspaceId(inv.ctx);
        if (!wsId) {
          if (!guarded) {
            await maybePost(inv, { status: "error", message: "no workspace" });
          }
          return;
        }
        const result = await dispatch(inv, wsId, t as Translate);
        if (result.status === "ok") {
          toast(t("repoActions.addedToast", { name: result.name }));
          try {
            await refreshReposAndMap(wsId);
          } catch {
            // The repo is already registered; a later workspace refresh will recover.
          }
          try {
            await refreshNeeds();
          } catch {
            // The canonical queue also polls; a later refresh removes the settled card.
          }
        } else if (result.status === "error") {
          toast(t("repoActions.failedToast", { message: result.message }));
        } else if (result.status === "stale") {
          toast(result.message);
          // A stale card is no longer an authorized lead action. Do not inject
          // a synthetic repo_action result into the newer conversation turn.
          return;
        } else if (result.status === "in_progress") {
          // Another surface/process owns this exact execution token. It will
          // refresh Needs and notify the lead; this loser is not a failure and
          // must not inject a duplicate result.
          return;
        }
        if (guarded) {
          // Guarded card execution persists one authoritative feedback outbox
          // in the same backend transaction that resolves the card. Every UI
          // surface, including the fresh winner, must stay out of that path.
          return;
        }
        await maybePost(inv, { ...result, workspace_id: wsId });
      } finally {
        setBusyFor(inv.actionId, false);
      }
    },
    [maybePost, refreshNeeds, refreshReposAndMap, resolveWorkspaceId, setBusyFor, t],
  );

  return { run, busy };
}

async function dispatch(
  inv: RepoActionInvocation,
  workspaceId: number,
  t: Translate,
): Promise<RepoActionResult> {
  let guard:
    | { threadId: number; messageId: number; actionId: string; actionKind: string }
    | undefined;
  if (inv.ctx.threadId != null && inv.ctx.messageId != null) {
    guard = {
      threadId: inv.ctx.threadId,
      messageId: inv.ctx.messageId,
      actionId: inv.actionId,
      actionKind: inv.kind,
    };
  }
  if (inv.kind === "add") {
    const dir = await openDialog({ directory: true, multiple: false });
    if (!dir || typeof dir !== "string") return { status: "cancelled" };
    try {
      const r = await api.addRepoRef(workspaceId, basename(dir), dir, guard);
      return ok(r);
    } catch (e) {
      return repoActionError(e, t);
    }
  }

  if (inv.kind === "new") {
    const parent = await openDialog({ directory: true, multiple: false });
    if (!parent || typeof parent !== "string") return { status: "cancelled" };
    const name = await inv.promptText(
      t("repoActions.repoNameTitle"),
      t("repoActions.repoNamePlaceholder"),
    );
    if (!name) return { status: "cancelled" };
    try {
      const r = await api.createRepo(workspaceId, name, parent, guard);
      return ok(r);
    } catch (e) {
      return repoActionError(e, t);
    }
  }

  // clone
  const url = await inv.promptText(
    t("repoActions.repoUrlTitle"),
    t("repoActions.repoUrlPlaceholder"),
  );
  if (!url) return { status: "cancelled" };
  const parent = await openDialog({ directory: true, multiple: false });
  if (!parent || typeof parent !== "string") return { status: "cancelled" };
  // Backend clones into `<parent>/<name>`, so a name is required.
  const defaultName = repoNameFromUrl(url);
  const name = await inv.promptText(
    t("repoActions.repoNameTitle"),
    defaultName || t("repoActions.repoNamePlaceholder"),
  );
  if (!name) return { status: "cancelled" };
  try {
    const r = await api.cloneRepo(workspaceId, url, parent, name, guard);
    return ok(r);
  } catch (e) {
    return repoActionError(e, t);
  }
}

function repoActionError(error: unknown, t: Translate): RepoActionResult {
  const message = String(error);
  if (message.includes("action_card_stale")) {
    return { status: "stale", message: t("repoActions.staleCard") };
  }
  return { status: "error", message };
}

function ok(result: RepoActionCommandResult): RepoActionResult {
  if (result.execution_outcome === "in_progress") {
    return { status: "in_progress" };
  }
  const r = result.repo;
  if (!r) {
    return { status: "error", message: "repository action completed without a repository" };
  }
  return {
    status: "ok",
    execution_outcome: result.execution_outcome,
    repo_id: String(r.id),
    name: r.name,
    local_git_path: r.local_git_path,
  };
}

function basename(p: string): string {
  const trimmed = p.replace(/[\\/]+$/, "");
  const parts = trimmed.split(/[\\/]/);
  return parts[parts.length - 1] || p;
}
