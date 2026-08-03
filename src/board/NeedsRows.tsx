import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import {
  AlertTriangle,
  ArrowUpRight,
  Check,
  ClipboardCheck,
  GitBranch,
  HelpCircle,
  Layers,
  RefreshCw,
  Send,
} from "lucide-react";
import type {
  AttentionItem,
  PermissionAsk,
  PrTrackingRetryAttentionItem,
  QuestionAttentionItem,
} from "../lib/types";
import { api } from "../lib/api";
import { cn } from "../lib/cn";
import { useStore } from "../state/store";
import { Button } from "../components/ui/Button";
import { Input } from "../components/ui/Input";
import { PermissionConfirmationCard } from "../components/ConfirmationCard";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { useRepoActions } from "../session/useRepoActions";
import { attentionAge } from "./attentionTime";

type PromptState = {
  title: string;
  placeholder?: string;
  value: string;
  resolve: (value: string | null) => void;
};

export function AttentionRow({ item }: { item: AttentionItem }) {
  switch (item.kind) {
    case "permission":
      return <PermissionRow ask={item.ask} />;
    case "question":
      return <QuestionRow item={item} />;
    case "plan_approval":
      return <PlanApprovalRow item={item} />;
    case "scope_approval":
      return <ScopeApprovalRow item={item} />;
    case "repo_action":
      return <RepoActionRow item={item} />;
    case "pr_tracking_retry":
      return <PrRetryRow item={item} />;
  }
}

export function PermissionRow({ ask }: { ask: PermissionAsk }) {
  const { answerPermission, releaseSessionReadOnly, goToDirectionRef } = useStore();
  const { t } = useTranslation();
  const context = [ask.thread_title, ask.dir_name].filter(Boolean).join(" · ");
  const contextLink = context ? (
    <button
      type="button"
      onClick={() => void goToDirectionRef(ask.thread, ask.dir)}
      title={t("needs.openDirection")}
      className="group flex max-w-full items-center gap-1.5 pt-0.5 text-[11px] text-ink-faint transition-colors hover:text-ink"
    >
      <Layers size={11} className="shrink-0" />
      <span className="truncate">{context}</span>
      <ArrowUpRight size={11} className="shrink-0 opacity-0 transition-opacity group-hover:opacity-100" />
    </button>
  ) : null;

  return (
    <PermissionConfirmationCard
      ask={ask}
      onAnswer={(askId, answer) => void answerPermission(askId, answer)}
      onReleaseSessionReadOnly={() => void releaseSessionReadOnly(ask.thread, ask.dir)}
      className="overflow-hidden rounded-[var(--radius-lg)] border-waiting/40 bg-waiting/10 px-3.5 pb-0 pt-3"
      actionsClassName="-mx-3.5 mt-1 self-stretch border-t border-border bg-bg/40 px-3.5 py-2.5"
      context={contextLink}
      timestamp={<span className="ml-auto whitespace-nowrap text-ink-faint tabular-nums">{agoSeconds(ask.ts, t)}</span>}
      showToolIcon
      summaryMode="block"
    />
  );
}

function QuestionRow({ item }: { item: QuestionAttentionItem }) {
  const { answerAsk, goToAsk } = useStore();
  const { t } = useTranslation();
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);

  async function submit() {
    if (!text.trim() || busy) return;
    setBusy(true);
    try {
      await answerAsk(item, text);
    } finally {
      setBusy(false);
    }
  }

  return (
    <ActionCard
      icon={<HelpCircle size={13} className="text-waiting" />}
      title={item.direction_name || t("needs.question")}
      context={item.thread_title}
      createdAt={item.created_at}
      onOpen={() => void goToAsk(item)}
    >
      <p className="px-3.5 pb-3 pt-1.5 text-[14px] leading-relaxed text-ink">{item.text}</p>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          void submit();
        }}
        className="flex gap-2 border-t border-border bg-bg/40 px-3.5 py-2.5"
      >
        <Input
          autoFocus
          placeholder={t("needs.answerPlaceholder", { name: item.direction_name })}
          value={text}
          onChange={(event) => setText(event.currentTarget.value)}
        />
        <Button type="submit" variant="primary" size="icon" disabled={!text.trim() || busy}>
          <Send size={14} />
        </Button>
      </form>
    </ActionCard>
  );
}

function PlanApprovalRow({ item }: { item: Extract<AttentionItem, { kind: "plan_approval" }> }) {
  const { approvePlanCard, refreshNeeds, selectThread } = useStore();
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);

  async function approve() {
    if (busy) return;
    setBusy(true);
    try {
      await approvePlanCard(item.thread_id, item.message_id);
      await refreshNeeds();
    } finally {
      setBusy(false);
    }
  }

  return (
    <ActionCard
      icon={<ClipboardCheck size={13} className="text-approval" />}
      title={t("needs.planApproval")}
      context={item.thread_title}
      createdAt={item.created_at}
      onOpen={() => void selectThread(item.thread_id)}
    >
      {item.title && <p className="px-3.5 pb-3 pt-1.5 text-[14px] text-ink">{item.title}</p>}
      <CardActions>
        <Button variant="primary" disabled={busy} onClick={() => void approve()}>
          <Check size={13} />
          {t("needs.approvePlan")}
        </Button>
      </CardActions>
    </ActionCard>
  );
}

function ScopeApprovalRow({ item }: { item: Extract<AttentionItem, { kind: "scope_approval" }> }) {
  const { selectThread, setReviewingProposal } = useStore();
  const { t } = useTranslation();

  async function review() {
    await selectThread(item.thread_id);
    setReviewingProposal(true);
  }

  return (
    <ActionCard
      icon={<Layers size={13} className="text-approval" />}
      title={t("needs.scopeApproval")}
      context={item.thread_title}
      createdAt={item.created_at}
      onOpen={() => void review()}
    >
      <CardActions>
        <Button variant="primary" onClick={() => void review()}>
          {t("needs.reviewScope")}
          <ArrowUpRight size={13} />
        </Button>
      </CardActions>
    </ActionCard>
  );
}

function RepoActionRow({ item }: { item: Extract<AttentionItem, { kind: "repo_action" }> }) {
  const { activeWorkspaceId, selectThread, setThreadTab } = useStore();
  const { t } = useTranslation();
  const { run, busy } = useRepoActions();
  const [promptState, setPromptState] = useState<PromptState | null>(null);

  const promptText = (title: string, placeholder?: string) =>
    new Promise<string | null>((resolve) => {
      setPromptState({ title, placeholder, value: "", resolve });
    });

  async function open() {
    await selectThread(item.thread_id);
    setThreadTab("lead");
  }

  async function invoke(action: (typeof item.actions)[number]) {
    await run({
      actionId: action.id,
      kind: action.kind,
      ctx: {
        threadId: item.thread_id,
        messageId: item.message_id,
        preferredWorkspaceId: activeWorkspaceId,
      },
      promptText,
    });
  }

  return (
    <ActionCard
      icon={<GitBranch size={13} className="text-approval" />}
      title={item.title || t("needs.repoAction")}
      context={item.thread_title}
      createdAt={item.created_at}
      onOpen={() => void open()}
    >
      <div className="flex flex-wrap gap-2 px-3.5 pb-3 pt-2">
        {item.actions.map((action) => (
          <Button
            key={action.id}
            variant="default"
            disabled={busy[action.id]}
            onClick={() => void invoke(action)}
          >
            <GitBranch size={13} />
            {action.label}
          </Button>
        ))}
      </div>
      <CardActions>
        <Button variant="ghost" onClick={() => void open()}>
          {t("needs.reviewRepoActions")}
          <ArrowUpRight size={13} />
        </Button>
      </CardActions>
      <PromptDialog state={promptState} setState={setPromptState} />
    </ActionCard>
  );
}

function PromptDialog({
  state,
  setState,
}: {
  state: PromptState | null;
  setState: (state: PromptState | null) => void;
}) {
  const { t } = useTranslation();
  return (
    <Dialog
      open={state != null}
      onOpenChange={(open) => {
        if (!open && state) {
          state.resolve(null);
          setState(null);
        }
      }}
    >
      {state && (
        <DialogContent title={state.title}>
          <form
            className="flex flex-col gap-3"
            onSubmit={(event) => {
              event.preventDefault();
              const value = state.value.trim();
              state.resolve(value || null);
              setState(null);
            }}
          >
            <Input
              autoFocus
              placeholder={state.placeholder}
              value={state.value}
              onChange={(event) => setState({ ...state, value: event.currentTarget.value })}
            />
            <div className="flex justify-end gap-2">
              <Button
                type="button"
                variant="ghost"
                onClick={() => {
                  state.resolve(null);
                  setState(null);
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
  );
}

function PrRetryRow({ item }: { item: PrTrackingRetryAttentionItem }) {
  const { retryPrTracking } = useStore();
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);

  async function retry() {
    if (busy) return;
    setBusy(true);
    try {
      await retryPrTracking(item);
    } finally {
      setBusy(false);
    }
  }

  return (
    <ActionCard
      icon={<AlertTriangle size={13} className="text-danger" />}
      title={item.title || t("needs.pullRequestNumber", { number: item.number })}
      context={[item.thread_title, item.direction_name].filter(Boolean).join(" · ")}
      createdAt={item.created_at}
      onOpen={() => void api.openUrl(item.url)}
      tone="danger"
    >
      <p className="px-3.5 pb-3 pt-1.5 text-[13px] leading-relaxed text-ink-muted">{item.error}</p>
      <CardActions>
        <Button variant="primary" disabled={busy} onClick={() => void retry()}>
          <RefreshCw size={13} className={cn(busy && "animate-spin")} />
          {t("needs.retryTracking")}
        </Button>
        <Button variant="ghost" className="ml-auto" onClick={() => void api.openUrl(item.url)}>
          {t("needs.openPullRequest")}
          <ArrowUpRight size={13} />
        </Button>
      </CardActions>
    </ActionCard>
  );
}

function ActionCard({
  icon,
  title,
  context,
  createdAt,
  onOpen,
  tone = "waiting",
  children,
}: {
  icon: ReactNode;
  title: string;
  context: string;
  createdAt: string | null;
  onOpen: () => void;
  tone?: "waiting" | "danger";
  children: ReactNode;
}) {
  const { t } = useTranslation();
  const border = tone === "danger" ? "border-danger/40" : "border-waiting/40";
  const age = agoIso(createdAt, t);
  return (
    <div className={cn("overflow-hidden rounded-[var(--radius-lg)] border bg-waiting/10", border)}>
      <div className="flex items-center gap-2 px-3.5 pt-3 text-[12px]">
        {icon}
        <button type="button" onClick={onOpen} className="group flex min-w-0 items-center gap-1.5 text-left" title={t("needs.openDirection")}>
          <span className="truncate font-medium text-ink transition-colors group-hover:text-brand">{title}</span>
          {context && <span className="truncate text-ink-muted">· {context}</span>}
        </button>
        <div className="ml-auto flex shrink-0 items-center gap-2">
          {age ? <span className="whitespace-nowrap text-ink-faint tabular-nums">{age}</span> : null}
          <button type="button" onClick={onOpen} aria-label={t("needs.openDirection")} className="-mr-1 grid h-6 w-6 place-items-center rounded text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink">
            <ArrowUpRight size={14} />
          </button>
        </div>
      </div>
      {children}
    </div>
  );
}

function CardActions({ children }: { children: ReactNode }) {
  return <div className="flex items-center gap-2 border-t border-border bg-bg/40 px-3.5 py-2.5">{children}</div>;
}

export function EmptyNeeds() {
  const { t } = useTranslation();
  return (
    <div className="flex h-full flex-col items-center justify-center px-6 text-center">
      <div className="grid h-12 w-12 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
        <Check size={22} className="text-running" />
      </div>
      <h2 className="mt-4 text-[15px] font-semibold text-ink">{t("needs.emptyTitle")}</h2>
      <p className="mt-1.5 max-w-sm text-[13px] leading-relaxed text-ink-faint">{t("needs.emptyBody")}</p>
    </div>
  );
}

function agoSeconds(ts: number, t: TFunction): string {
  return agoFromMilliseconds(ts * 1000, t);
}

function agoIso(ts: string | null, t: TFunction): string | null {
  const age = attentionAge(ts);
  if (age == null) return null;
  switch (age.kind) {
    case "just_now":
      return t("time.justNow");
    case "minutes":
      return t("time.mAgo", { n: age.value });
    case "hours":
      return t("time.hAgo", { n: age.value });
    case "days":
      return t("time.dAgo", { n: age.value });
  }
}

function agoFromMilliseconds(ts: number, t: TFunction): string {
  const seconds = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (seconds < 60) return t("time.justNow");
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return t("time.mAgo", { n: minutes });
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return t("time.hAgo", { n: hours });
  return t("time.dAgo", { n: Math.floor(hours / 24) });
}
