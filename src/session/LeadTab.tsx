import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Info } from "lucide-react";
import { useStore } from "../state/store";
import { ChatTimeline } from "./ChatTimeline";
import { ChatComposer } from "./ChatComposer";
import { PermissionBar } from "./PermissionBar";
import { SessionInfoPanel } from "./SessionInfoPanel";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { Input } from "../components/ui/Input";
import { Button } from "../components/ui/Button";
import { ToolIcon, toolFullName } from "../components/ToolIcon";
import { useRepoActions } from "./useRepoActions";
import { api } from "../lib/api";
import type { EnabledSkill } from "../lib/types";
import { resumeCommand } from "../lib/resume";

type PromptState = {
  title: string;
  placeholder?: string;
  value: string;
  resolve: (v: string | null) => void;
};

// Host-owned local slash items. ChatComposer keeps the "what" generic
// (a name + label); the kind is mapped to a useRepoActions invocation here.
const LOCAL_SLASH = [
  { name: "add-repo", kind: "add" as const, labelKey: "slashLocal.addRepo" },
  { name: "new-repo", kind: "new" as const, labelKey: "slashLocal.newRepo" },
  { name: "clone-repo", kind: "clone" as const, labelKey: "slashLocal.cloneRepo" },
];

/**
 * The issue console — a real chat, not a projection of the CLI's log. Messages
 * live in weft's own store, replies stream token-by-token over the lead-chat
 * event, and structured cards sit inline in the timeline. The engine survives
 * restarts (resume) so history is always here and the composer always works.
 */
export function LeadTab({
  onReview,
  threadId,
  compact = false,
}: {
  onReview: () => void;
  threadId?: number;
  compact?: boolean;
}) {
  const {
    activeThreadId,
    activeWorkspaceId,
    threads,
    leadMessages,
    leadTurn,
    leadSlash,
    leadActivity,
    leadMeta,
    directionsByThread,
    repos,
    loadLeadChat,
    discoverLeadSlash,
    sendLeadChat,
    interruptLead,
    setReviewingProposal,
    proposal,
    asks,
    skillsDirtyAt,
    markSkillsDirty,
    mergeLeadMeta,
  } = useStore();
  const { t } = useTranslation();
  const { run, busy: actionsBusy } = useRepoActions();
  const [promptState, setPromptState] = useState<PromptState | null>(null);
  const [rail, setRail] = useState<"info" | "none">(compact ? "none" : "info");
  const [skills, setSkills] = useState<EnabledSkill[]>([]);
  // The lead's working dir — resolves relative file paths it mentions in chat.
  const [leadCwd, setLeadCwd] = useState<string | undefined>(undefined);

  // The thread this chat renders. Defaults to the globally-active thread (the
  // ThreadBoard usage); the embedded curator panel passes its own thread id so
  // it renders without touching navigation. All lead store slices are keyed by
  // thread id, so only the source of the id changes.
  const tid = threadId ?? activeThreadId;

  const promptText = (title: string, placeholder?: string) =>
    new Promise<string | null>((resolve) =>
      setPromptState({ title, placeholder, value: "", resolve }),
    );

  // Stable identity per language so ChatComposer's slashMatches useMemo
  // doesn't recompute on every parent render.
  const localSlash = useMemo(
    () => LOCAL_SLASH.map((c) => ({ name: c.name, label: t(c.labelKey) })),
    [t],
  );

  useEffect(() => {
    if (tid != null) void loadLeadChat(tid);
  }, [tid, loadLeadChat]);

  // Enabled skills for the panel (workspace-level). Re-fetch when skills change.
  useEffect(() => {
    if (activeWorkspaceId == null) return;
    void api
      .workspaceSkills(activeWorkspaceId)
      .then((s) => setSkills(s.filter((x) => !x.overridden)))
      .catch(() => {});
  }, [activeWorkspaceId, skillsDirtyAt]);

  // 非-claude lead 的带外 meta(model/window/MCP server)。claude lead 命令返回 null →
  // 不并入(事件流 init/usage 已填,别被空快照覆盖)。开页 + turn 结束 + 重载各拉一次。
  useEffect(() => {
    if (tid == null) return;
    // 按 turn state 触发,running/idle 都会跑;用 alive 丢弃被取代的旧请求,避免
    // running 期请求晚于 idle 期返回而用旧 meta 盖掉新值(也防 thread 切换串台)。
    let alive = true;
    void api
      .leadSessionMeta(tid)
      .then((s) => {
        if (alive && s) mergeLeadMeta(tid, s);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [tid, leadTurn[tid ?? -1]?.state, skillsDirtyAt, mergeLeadMeta]);

  useEffect(() => {
    // Drop the previous thread's cwd immediately — otherwise a relative file
    // ref clicked during the fetch window would resolve against the old lead
    // workspace. Undefined cwd fails safe (relative paths report not-found).
    setLeadCwd(undefined);
    if (tid == null) return;
    let alive = true;
    void api
      .leadState(tid)
      .then((st) => {
        if (alive) setLeadCwd(st.cwd);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [tid]);

  if (tid == null) return null;
  // The lead's own timeline: worker chat rows carry a session_id, skip them.
  const msgs = (leadMessages[tid] ?? []).filter((m) => m.session_id == null);
  const turn = leadTurn[tid] ?? { state: "stopped" as const, queue: [] };
  // The lead engine runs the thread's lead_tool (not always claude).
  const leadTool = threads.find((th) => th.id === tid)?.lead_tool ?? "claude";

  // 重载会话:重拉 skills + 标记静默 re-spawn(claude 下条消息拾取新 MCP/skill)。
  const onReload = () => {
    markSkillsDirty();
    void api.flagLeadSkillRefresh(tid);
  };

  const onLocalSlash = (name: string) => {
    const item = LOCAL_SLASH.find((x) => x.name === name);
    if (!item) return;
    void run({
      actionId: `local-${item.kind}-${Date.now()}`,
      kind: item.kind,
      ctx: {
        threadId: tid,
        preferredWorkspaceId: activeWorkspaceId,
      },
      promptText,
    });
  };

  return (
    <div className="flex min-h-0 min-w-0 flex-1">
      <section className="flex min-w-0 flex-1 flex-col bg-bg">
        {!compact && (
          <header className="flex items-center gap-2 border-b border-border bg-surface px-3 py-2">
            <span className="mr-auto flex shrink-0 items-center gap-1.5 whitespace-nowrap rounded-[var(--radius-sm)] bg-bg px-2 py-0.5 text-[11px] font-medium text-ink-muted">
              <ToolIcon tool={leadTool} size={12} />
              {toolFullName(leadTool)}
            </span>
            <button
              onClick={() => setRail((r) => (r === "info" ? "none" : "info"))}
              title={t("sessionInfo.title")}
              aria-label={t("sessionInfo.title")}
              className={`grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] border transition-colors ${
                rail === "info"
                  ? "border-brand bg-brand-ghost text-brand"
                  : "border-border text-ink-muted hover:bg-surface hover:text-ink"
              }`}
            >
              <Info size={13} />
            </button>
          </header>
        )}
        <PermissionBar
          asks={asks.filter((a) => a.thread === tid && (a.dir === "lead" || a.dir === ""))}
        />
        <ChatTimeline
          messages={msgs}
          busy={turn.state === "busy"}
          activity={leadActivity[tid]}
          onReviewProposal={() => {
            setReviewingProposal(true);
            onReview();
          }}
          proposal={proposal}
          runAction={run}
          actionsBusy={actionsBusy}
          threadId={tid}
          workspaceId={activeWorkspaceId}
          promptText={promptText}
          cwd={leadCwd}
          emptyState={repos.length === 0 ? "lead-repo-guide" : "lead-task"}
          queue={turn.queue}
          onRemove={(id) => void api.leadDequeue(tid, id)}
          onEdit={(id, text) => void api.leadEditQueued(tid, id, text)}
          onReorder={(order) => void api.leadReorderQueue(tid, order)}
        />
        <ChatComposer
          slashCommands={leadSlash[tid] ?? []}
          localSlash={localSlash}
          onLocalSlash={onLocalSlash}
          busy={turn.state === "busy"}
          queued={turn.queue.length}
          onSend={(text, images, files) =>
            sendLeadChat(tid, text, images, files)
          }
          onStop={() => void interruptLead(tid)}
          onNeedSlashCommands={() => discoverLeadSlash(tid)}
          onTakeOver={async () => {
            const st = await api.leadState(tid);
            if (!st.native_id) return false;
            await api.leadStop(tid);
            await navigator.clipboard.writeText(
              resumeCommand(leadTool, st.cwd, st.native_id, st.command),
            );
            return true;
          }}
        />
        <Dialog
          open={promptState != null}
          onOpenChange={(open) => {
            if (!open && promptState) {
              promptState.resolve(null);
              setPromptState(null);
            }
          }}
        >
          {promptState && (
            <DialogContent title={promptState.title}>
              <form
                onSubmit={(e) => {
                  e.preventDefault();
                  const v = promptState.value.trim();
                  promptState.resolve(v || null);
                  setPromptState(null);
                }}
                className="flex flex-col gap-3"
              >
                <Input
                  autoFocus
                  placeholder={promptState.placeholder}
                  value={promptState.value}
                  onChange={(e) =>
                    setPromptState((s) => (s ? { ...s, value: e.currentTarget.value } : s))
                  }
                />
                <div className="flex justify-end gap-2">
                  <Button
                    type="button"
                    variant="ghost"
                    onClick={() => {
                      promptState.resolve(null);
                      setPromptState(null);
                    }}
                  >
                    {t("session.promptCancel")}
                  </Button>
                  <Button type="submit" variant="primary">
                    {t("session.promptOk")}
                  </Button>
                </div>
              </form>
            </DialogContent>
          )}
        </Dialog>
      </section>

      {rail === "info" && (
        <SessionInfoPanel
          meta={leadMeta[tid]}
          skills={skills}
          subtasks={directionsByThread[tid]}
          onClose={() => setRail("none")}
          onReload={onReload}
          busy={turn.state === "busy"}
        />
      )}
    </div>
  );
}
