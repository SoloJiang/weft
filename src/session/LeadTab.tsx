import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { currentLang } from "../i18n";
import { useStore } from "../state/store";
import { ChatTimeline } from "./ChatTimeline";
import { LeadEmptyState } from "./LeadEmptyState";
import { ChatComposer, type LocalSlashSpec } from "./ChatComposer";
import { SessionInfoPanel } from "./SessionInfoPanel";
import { TestPlanPanel, testPlanCaseCount } from "./TestPlanPanel";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { Input } from "../components/ui/Input";
import { Button } from "../components/ui/Button";
import { RewindDialog, RewindPickerDialog } from "./RewindDialog";
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

// Host-owned local slash items, mapped to the composer's LocalSlashSpec below.
// Two shapes: repo-onboarding items run a useRepoActions flow (act "action" in
// the composer); a "prompt" item carries a canned message the composer sends
// through its own send path — used to make an otherwise-invisible soft policy
// (deriving test cases) discoverable and explicitly triggerable from the slash
// palette. Prompt items are issue-lead only (filtered out of the curator panel).
type LocalSlashItem =
  | { name: string; act: "repo"; kind: "add" | "new" | "clone"; labelKey: string }
  | { name: string; act: "prompt"; labelKey: string; promptKey: string };

const LOCAL_SLASH: LocalSlashItem[] = [
  { name: "test-cases", act: "prompt", labelKey: "slashLocal.testCases", promptKey: "slashLocal.testCasesPrompt" },
  { name: "add-repo", act: "repo", kind: "add", labelKey: "slashLocal.addRepo" },
  { name: "new-repo", act: "repo", kind: "new", labelKey: "slashLocal.newRepo" },
  { name: "clone-repo", act: "repo", kind: "clone", labelKey: "slashLocal.cloneRepo" },
];

/**
 * The issue console — a real chat, not a projection of the CLI's log. Messages
 * live in weft's own store, replies stream token-by-token over the lead-chat
 * event, and structured cards sit inline in the timeline. The engine survives
 * restarts (resume) so history is always here and the composer always works.
 */
export function LeadTab({
  threadId,
  compact = false,
  composePlaceholder,
  emptyState,
}: {
  threadId?: number;
  compact?: boolean;
  /** Composer placeholder override — the curator panel passes its own so the
   *  embedded chat doesn't read "给 lead 发消息…" (lead-chat jargon). */
  composePlaceholder?: string;
  /** Empty-state slot override — the curator panel injects its own node so its
   *  embedded chat doesn't show the issue console's task-planning cue. Defaults to
   *  the issue-console LeadEmptyState built below. */
  emptyState?: ReactNode;
}) {
  const {
    activeThreadId,
    activeWorkspaceId,
    threads,
    leadMessages,
    leadHistoryStatus,
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
    leadRail,
    setLeadRail,
    viewDirection,
  } = useStore();
  const { t } = useTranslation();
  const { run, busy: actionsBusy } = useRepoActions();
  const [promptState, setPromptState] = useState<PromptState | null>(null);
  // The right rail is a store toggle (info flips from the top bar — this
  // surface is header-less; tests opens from a test-cases card); the embedded
  // curator panel (compact) never shows either.
  const rail = compact ? "none" : leadRail;
  const [skills, setSkills] = useState<EnabledSkill[]>([]);
  // The lead's working dir — resolves relative file paths it mentions in chat.
  const [leadCwd, setLeadCwd] = useState<string | undefined>(undefined);
  // Live test-case count for the plan card, sourced from the test_plan table so
  // it matches what the panel's View opens. Bumped by a user panel edit (which
  // rewrites the table WITHOUT a test_cases card); lead re-emits refetch via the
  // test_cases card id below.
  const [testCaseCount, setTestCaseCount] = useState(0);
  const [testPlanEditNonce, setTestPlanEditNonce] = useState(0);
  // Conversation rewind: the message id awaiting confirm (null = dialog closed),
  // the Esc-Esc picker's open flag, and the composer prefill (seq bumps to
  // remount-inject the rewound text). The lead rewinds conversation-only.
  const [rewindId, setRewindId] = useState<number | null>(null);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [prefill, setPrefill] = useState<{ text: string; seq: number }>({ text: "", seq: 0 });

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
  // doesn't recompute on every parent render. Prompt items are issue-lead only:
  // hide them from the compact curator panel (a hidden curator thread) where an
  // issue-specific ask would go to the wrong agent. Repo items stay in both.
  const localSlash = useMemo<LocalSlashSpec[]>(() => {
    const toSpec = (c: LocalSlashItem): LocalSlashSpec => {
      if (c.act === "prompt") {
        return { name: c.name, label: t(c.labelKey), act: "prompt", prompt: t(c.promptKey) };
      }
      return { name: c.name, label: t(c.labelKey), act: "action" };
    };
    return LOCAL_SLASH.filter((c) => c.act !== "prompt" || !compact).map(toSpec);
  }, [t, compact]);

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

  // 带外 meta:codex/opencode 补 model/window/MCP server;claude 只补 cwd 的 skill(其余
  // 走事件流 init/usage,空字段不覆盖)。命令返回 null 时不并入。开页 + turn 状态变 +
  // 重载(skillsDirtyAt)各拉一次。
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

  // The latest test_cases card id only grows (append-only timeline). The lead
  // ALWAYS emits a test_cases card when it (re)writes the test_plan table, so
  // this id tracks every lead-side write; bumping the open panel makes it
  // refetch too. (test_cases rows are lead-only, so the unfiltered messages give
  // the same id as the visible timeline.)
  const testPlanRefreshKey = (leadMessages[tid ?? -1] ?? []).reduce(
    (acc, m) => (m.kind === "test_cases" ? m.id : acc),
    0,
  );

  // Recount the plan card against the LIVE test plan on thread switch, lead
  // re-emit (refreshKey), and user panel edit (editNonce) — the card must show
  // the same count the View button opens, even after an edit that posts no
  // test_cases card. `alive` drops responses raced by a newer trigger/thread.
  useEffect(() => {
    if (tid == null) {
      setTestCaseCount(0);
      return;
    }
    let alive = true;
    void api
      .getTestPlan(tid)
      .then((p) => {
        if (alive) setTestCaseCount(p ? testPlanCaseCount(p.content) : 0);
      })
      .catch(() => {
        if (alive) setTestCaseCount(0);
      });
    return () => {
      alive = false;
    };
  }, [tid, testPlanRefreshKey, testPlanEditNonce]);

  if (tid == null) return null;
  // The lead's own timeline: worker chat rows carry a session_id, skip them.
  const msgs = (leadMessages[tid] ?? []).filter((m) => m.session_id == null);
  const turn = leadTurn[tid] ?? { state: "stopped" as const, queue: [] };
  // The lead engine runs the thread's lead_tool (not always claude).
  const leadTool = threads.find((th) => th.id === tid)?.lead_tool ?? "claude";
  // Rewind is scoped to claude/codex/opencode leads — the tools with native
  // fork support (same gate as the worker); the lead rewinds conversation-only.
  const canRewind = leadTool === "claude" || leadTool === "codex" || leadTool === "opencode";

  // Dialog confirm: the backend truncates from the picked message on and
  // returns its text — prefill the composer with it. The "rewound" push
  // reloads the timeline independently (store listener).
  const confirmRewind = async () => {
    if (rewindId == null) return;
    // lang mirrors lead_send: a rewind-triggered engine rebuild (cold start)
    // would otherwise stick the lead to the default "en" for this run.
    const r = await api.leadRewind(tid, rewindId, currentLang());
    setPrefill((p) => ({ text: r.rewound_text, seq: p.seq + 1 }));
  };

  // 重载会话:先注入新启用的 skill 到 lead cwd(并标记静默 re-spawn,claude 下条消息拾取),
  // **注入完成后**再 bump skillsDirtyAt,让带外 meta effect 重扫 cwd —— 避免重扫抢在
  // inject_for 之前把陈旧列表当权威合并。
  const onReload = () => {
    void api.flagLeadSkillRefresh(tid).finally(() => markSkillsDirty());
  };

  const onLocalSlash = (name: string) => {
    const item = LOCAL_SLASH.find((x) => x.name === name);
    // Only "action" (repo) items reach the host — the composer sends "prompt"
    // items itself (through its own send path: attachments + queue-full guard).
    if (!item || item.act !== "repo") return;
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
        <ChatTimeline
          messages={msgs}
          historyStatus={leadHistoryStatus[tid] ?? "loading"}
          timelineKey={`lead:${tid}`}
          onRetryHistory={() => void loadLeadChat(tid)}
          asks={asks.filter((a) => a.thread === tid && (a.dir === "lead" || a.dir === ""))}
          busy={turn.state === "busy"}
          activity={leadActivity[tid]}
          onReviewProposal={() => setReviewingProposal(true)}
          proposal={proposal}
          runAction={run}
          actionsBusy={actionsBusy}
          threadId={tid}
          workspaceId={activeWorkspaceId}
          promptText={promptText}
          cwd={leadCwd}
          emptyState={
            emptyState ?? (
              <LeadEmptyState
                mode={repos.length === 0 ? "lead-repo-guide" : "lead-task"}
                runAction={run}
                actionsBusy={actionsBusy}
                threadId={tid}
                workspaceId={activeWorkspaceId}
                promptText={promptText}
              />
            )
          }
          queue={turn.queue}
          onRemove={(id) => void api.leadDequeue(tid, id)}
          onEdit={(id, text) => void api.leadEditQueued(tid, id, text)}
          onReorder={(order) => void api.leadReorderQueue(tid, order)}
          onOpenTestPlan={compact ? undefined : () => setLeadRail("tests")}
          testCaseCount={testCaseCount}
          onRewind={canRewind ? (id) => setRewindId(id) : undefined}
        />
        <ChatComposer
          key={prefill.seq}
          initialValue={prefill.text}
          slashCommands={leadSlash[tid] ?? []}
          localSlash={localSlash}
          onLocalSlash={onLocalSlash}
          placeholder={composePlaceholder}
          tool={leadTool}
          contextMeta={leadMeta[tid]}
          busy={turn.state === "busy"}
          queued={turn.queue.length}
          onSend={(text, images, files) =>
            sendLeadChat(tid, text, images, files)
          }
          onStop={() => void interruptLead(tid)}
          onNeedSlashCommands={() => discoverLeadSlash(tid)}
          onRewindPicker={canRewind ? () => setPickerOpen(true) : undefined}
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
        <RewindDialog
          open={rewindId != null}
          onOpenChange={(o) => {
            if (!o) setRewindId(null);
          }}
          onConfirm={confirmRewind}
        />
        <RewindPickerDialog
          open={pickerOpen}
          onOpenChange={setPickerOpen}
          messages={msgs}
          onPick={(id) => {
            setPickerOpen(false);
            setRewindId(id);
          }}
        />
      </section>

      {rail === "info" && (
        <SessionInfoPanel
          meta={leadMeta[tid]}
          skills={skills}
          subtasks={directionsByThread[tid]}
          onOpenSubtask={(directionId) => {
            // Resolve the direction's live worktree → its worker surface. The
            // panel row was previously inert — the only path to a worker was
            // the board's lane menu.
            void api
              .listWorktrees(directionId)
              .then((writes) => {
                const first = writes.find((w) => w.exists);
                if (first) viewDirection(directionId, first.repo_id);
              })
              .catch(() => {});
          }}
          onClose={() => setLeadRail("none")}
          onReload={onReload}
          busy={turn.state === "busy"}
        />
      )}
      {rail === "tests" && (
        <TestPlanPanel
          // Key by thread: switching issues remounts the panel, so issue A's
          // edit mode/draft can never be saved into issue B.
          key={tid}
          threadId={tid}
          refreshKey={testPlanRefreshKey}
          onClose={() => setLeadRail("none")}
          onSendToLead={(text) => void sendLeadChat(tid, text, [], [])}
          onEdited={() => setTestPlanEditNonce((n) => n + 1)}
        />
      )}
    </div>
  );
}
