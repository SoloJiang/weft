import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import {
  ArrowUpRight,
  Check,
  GitBranch,
  Layers,
  Send,
  X,
} from "lucide-react";
import type { NeedItem, PermissionAsk, WriteTrigger } from "../lib/types";
import { cn } from "../lib/cn";
import { useStore } from "../state/store";
import { Button } from "../components/ui/Button";
import { Input } from "../components/ui/Input";
import { ToolIcon, toolFullName } from "../components/ToolIcon";
import { PermissionConfirmationCard } from "../components/ConfirmationCard";

export function WriteTriggerRow({ item }: { item: WriteTrigger }) {
  const { approveWriteTrigger, denyWriteTrigger, selectThread, defaultTool, installedTools } =
    useStore();
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [picked, setPicked] = useState<string | null>(null);
  const tool = picked ?? defaultTool;
  const installed = installedTools.filter((tl) => tl.installed);
  const context = [item.thread_title, item.name].filter(Boolean).join(" · ");

  async function act(fn: () => Promise<void>) {
    if (busy) return;
    setBusy(true);
    try {
      await fn();
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="overflow-hidden rounded-[var(--radius-lg)] border border-approval/40 bg-waiting/10">
      <div className="flex items-center gap-2 px-3.5 pt-3 text-[12px]">
        <GitBranch size={13} className="shrink-0 text-approval" />
        <span className="text-ink-faint">{t("needs.wantsToWrite")}</span>
        <span className="font-mono font-medium text-ink">{item.repo_name}</span>
        {context && (
          <button
            type="button"
            onClick={() => void selectThread(item.thread_id)}
            title={t("needs.openDirection")}
            className="group ml-auto flex min-w-0 items-center gap-1.5 text-ink-faint transition-colors hover:text-ink"
          >
            <Layers size={11} className="shrink-0" />
            <span className="truncate">{context}</span>
          </button>
        )}
      </div>
      <p className="px-3.5 pb-1 pt-1.5 text-[14px] leading-relaxed text-ink">
        {item.reason}
      </p>
      {item.base_branch && (
        <div className="px-3.5 pb-2">
          <span
            title={t("scope.baseBranch")}
            className="inline-flex items-center gap-1 rounded-full border border-border bg-bg px-2 py-0.5 text-[10.5px] font-mono text-ink-faint"
          >
            <GitBranch size={10} />
            {item.base_branch}
          </span>
        </div>
      )}
      <div className="flex flex-wrap items-center gap-2 border-t border-border bg-bg/40 px-3.5 py-2.5">
        <Button
          variant="primary"
          disabled={busy}
          title={t("needs.approveRunTitle")}
          onClick={() => void act(() => approveWriteTrigger(item, tool))}
        >
          <Check size={13} />
          {t("needs.approveRun")}
        </Button>
        {installed.length > 1 && (
          <div
            title={t("needs.runWith")}
            className="inline-flex items-center gap-0.5 rounded-[var(--radius-md)] bg-bg p-0.5"
          >
            {installed.map((tl) => (
              <button
                key={tl.tool}
                type="button"
                title={toolFullName(tl.tool)}
                onClick={() => setPicked(tl.tool)}
                className={cn(
                  "grid h-6 w-7 place-items-center rounded-[var(--radius-sm)] transition-opacity duration-150",
                  tool === tl.tool ? "bg-raised" : "opacity-40 hover:opacity-80",
                )}
              >
                <ToolIcon tool={tl.tool} size={13} />
              </button>
            ))}
          </div>
        )}
        <Button
          variant="ghost"
          className="ml-auto"
          disabled={busy}
          title={t("needs.denyWriteTitle")}
          onClick={() => void act(() => denyWriteTrigger(item))}
        >
          <X size={13} />
          {t("common.deny")}
        </Button>
      </div>
    </div>
  );
}

export function PermissionRow({ ask }: { ask: PermissionAsk }) {
  const { answerPermission, selectThread } = useStore();
  const { t } = useTranslation();
  const context = [ask.thread_title, ask.dir_name].filter(Boolean).join(" · ");
  const contextLink = context ? (
    <button
      type="button"
      onClick={() => void selectThread(ask.thread)}
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
      className="overflow-hidden rounded-[var(--radius-lg)] border-waiting/40 bg-waiting/10 px-3.5 pb-0 pt-3"
      actionsClassName="-mx-3.5 mt-1 self-stretch border-t border-border bg-bg/40 px-3.5 py-2.5"
      context={contextLink}
      timestamp={
        <span className="ml-auto whitespace-nowrap text-ink-faint tabular-nums">
          {ago(ask.ts, t)}
        </span>
      }
      showToolIcon
      summaryMode="block"
    />
  );
}

export function AskRow({ item }: { item: NeedItem }) {
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
    <div className="overflow-hidden rounded-[var(--radius-lg)] border border-waiting/40 bg-waiting/10">
      <div className="flex items-center gap-2 px-3.5 pt-3 text-[12px]">
        <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-waiting" />
        <span className="truncate font-medium text-ink">
          {item.direction_name}
        </span>
        <span className="text-ink-faint">·</span>
        <span className="truncate text-ink-muted">{item.thread_title}</span>
        <span className="ml-auto whitespace-nowrap text-ink-faint tabular-nums">
          {ago(item.ts, t)}
        </span>
        <button
          type="button"
          onClick={() => void goToAsk(item)}
          title={t("needs.openDirection")}
          aria-label={t("needs.openDirection")}
          className="-mr-1 grid h-6 w-6 shrink-0 place-items-center rounded text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
        >
          <ArrowUpRight size={14} />
        </button>
      </div>

      <p className="px-3.5 pb-3 pt-1.5 text-[14px] leading-relaxed text-ink">
        {item.text}
      </p>

      {item.answerable ? (
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
      ) : (
        // Display-only NOTICE (self-clearing stall hint): no answer box — it
        // retracts itself, and answering is refused backend-side.
        <p className="border-t border-border bg-bg/40 px-3.5 py-2.5 text-[12px] text-ink-faint">
          {t("needs.selfClearing")}
        </p>
      )}
    </div>
  );
}

export function EmptyNeeds() {
  const { t } = useTranslation();
  return (
    <div className="flex h-full flex-col items-center justify-center px-6 text-center">
      <div className="grid h-12 w-12 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
        <Check size={22} className="text-running" />
      </div>
      <h2 className="mt-4 text-[15px] font-semibold text-ink">{t("needs.emptyTitle")}</h2>
      <p className="mt-1.5 max-w-sm text-[13px] leading-relaxed text-ink-faint">
        {t("needs.emptyBody")}
      </p>
    </div>
  );
}

function ago(ts: number, t: TFunction): string {
  const s = Math.max(0, Math.floor(Date.now() / 1000) - ts);
  if (s < 60) return t("time.justNow");
  const m = Math.floor(s / 60);
  if (m < 60) return t("time.mAgo", { n: m });
  const h = Math.floor(m / 60);
  if (h < 24) return t("time.hAgo", { n: h });
  return t("time.dAgo", { n: Math.floor(h / 24) });
}
