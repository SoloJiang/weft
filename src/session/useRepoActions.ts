// Shared hook for the three repo-onboarding flows (add / new / clone).
// The hook owns workspace resolution, dialog/picker orchestration, toasts,
// and best-effort回灌 into the lead thread; the caller supplies the text
// prompt UI (Modal/inline form) via `promptText` so we keep zero JSX here.

import { useCallback, useState } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { useTranslation } from "react-i18next";

import { toast } from "../components/Toast";
import { useStore } from "../state/store";
import { api } from "../lib/api";
import type { RepoRef } from "../lib/types";

export type RepoActionKind = "add" | "new" | "clone";

export interface RepoActionContext {
  /** When present, the result is posted back to the lead thread. */
  threadId?: number;
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
  | { status: "ok"; repo_id: string; name: string; local_git_path: string }
  | { status: "error"; message: string }
  | { status: "cancelled" };

type Translate = (key: string, opts?: Record<string, unknown>) => string;

export function useRepoActions() {
  const { t } = useTranslation();
  const { activeWorkspaceId, refreshReposAndMap } = useStore();
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
        await api.postLeadToolResult(inv.ctx.threadId, full);
      } catch {
        // best-effort回灌; UI already toasted the user-visible outcome.
      }
    },
    [],
  );

  const run = useCallback(
    async (inv: RepoActionInvocation) => {
      setBusyFor(inv.actionId, true);
      try {
        const wsId = await resolveWorkspaceId(inv.ctx);
        if (!wsId) {
          await maybePost(inv, { status: "error", message: "no workspace" });
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
        } else if (result.status === "error") {
          toast(t("repoActions.failedToast", { message: result.message }));
        }
        await maybePost(inv, { ...result, workspace_id: wsId });
      } finally {
        setBusyFor(inv.actionId, false);
      }
    },
    [maybePost, refreshReposAndMap, resolveWorkspaceId, setBusyFor, t],
  );

  return { run, busy };
}

async function dispatch(
  inv: RepoActionInvocation,
  workspaceId: number,
  t: Translate,
): Promise<RepoActionResult> {
  if (inv.kind === "add") {
    const dir = await openDialog({ directory: true, multiple: false });
    if (!dir || typeof dir !== "string") return { status: "cancelled" };
    try {
      const r: RepoRef = await api.addRepoRef(workspaceId, basename(dir), dir);
      return ok(r);
    } catch (e) {
      return { status: "error", message: String(e) };
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
      const r: RepoRef = await api.createRepo(workspaceId, name, parent);
      return ok(r);
    } catch (e) {
      return { status: "error", message: String(e) };
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
    const r: RepoRef = await api.cloneRepo(workspaceId, url, parent, name);
    return ok(r);
  } catch (e) {
    return { status: "error", message: String(e) };
  }
}

function ok(r: RepoRef): RepoActionResult {
  return {
    status: "ok",
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

function repoNameFromUrl(url: string): string {
  const trimmed = url.trim().replace(/\.git$/i, "").replace(/[\\/]+$/, "");
  const parts = trimmed.split(/[\\/:]/);
  return parts[parts.length - 1] || "";
}
