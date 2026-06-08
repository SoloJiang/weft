import { useState } from "react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import { useTranslation } from "react-i18next";
import {
  ArrowUpRight,
  Check,
  CheckCheck,
  GitBranch,
  HelpCircle,
  Layers,
  Send,
  ShieldCheck,
  ShieldQuestion,
  X,
} from "lucide-react";
import { useStore } from "../state/store";
import type { NeedItem, PermissionAsk, WriteTrigger } from "../lib/types";
import { Button } from "../components/ui/Button";
import { Input } from "../components/ui/Input";
import type { TFunction } from "i18next";

/**
 * The "Needs-you" surface (PRODUCT §7): every open agent→human question across
 * the workspace, the one thing the human is here to handle. A pure projection of
 * the bus's ask channel — no TUI parsing. Answering routes the reply straight
 * back to the asking direction's inbox.
 */
export function NeedsYouView() {
  const { needs, asks, writeTriggers } = useStore();
  const { t } = useTranslation();
  const reduce = useReducedMotion();
  const total = needs.length + asks.length + writeTriggers.length;

  return (
    <section className="flex min-w-0 flex-1 flex-col bg-bg">
      <header className="flex items-center gap-2.5 border-b border-border px-5 py-3">
        <span className="grid h-6 w-6 place-items-center rounded-[var(--radius-sm)] bg-waiting/15">
          <HelpCircle size={14} className="text-waiting" />
        </span>
        <h1 className="text-[16px] font-semibold tracking-tight text-ink">
          {t("needs.title")}
        </h1>
        {total > 0 && (
          <span className="rounded-full bg-waiting/15 px-2 py-0.5 text-[11px] font-medium tabular-nums text-waiting">
            {total}
          </span>
        )}
        <span className="ml-auto text-[12px] text-ink-faint">
          {t("needs.subtitle")}
        </span>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto">
        {total === 0 ? (
          <EmptyNeeds />
        ) : (
          <div className="mx-auto flex w-full max-w-[680px] flex-col gap-2.5 px-5 py-5">
            <AnimatePresence initial={false}>
              {writeTriggers.map((wt) => (
                <motion.div
                  key={`wt-${wt.thread_id}-${wt.index}`}
                  layout={!reduce}
                  initial={reduce ? false : { opacity: 0, y: 6 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={reduce ? { opacity: 0 } : { opacity: 0, height: 0, marginBottom: -10, scale: 0.98 }}
                  transition={{ duration: 0.18, ease: [0.22, 1, 0.36, 1] }}
                >
                  <WriteTriggerRow item={wt} />
                </motion.div>
              ))}
              {asks.map((ask) => (
                <motion.div
                  key={`ask-${ask.id}`}
                  layout={!reduce}
                  initial={reduce ? false : { opacity: 0, y: 6 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={reduce ? { opacity: 0 } : { opacity: 0, height: 0, marginBottom: -10, scale: 0.98 }}
                  transition={{ duration: 0.18, ease: [0.22, 1, 0.36, 1] }}
                >
                  <PermissionRow ask={ask} />
                </motion.div>
              ))}
              {needs.map((item) => (
                <motion.div
                  key={`need-${item.ask_id}`}
                  layout={!reduce}
                  initial={reduce ? false : { opacity: 0, y: 6 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={
                    reduce
                      ? { opacity: 0 }
                      : { opacity: 0, height: 0, marginBottom: -10, scale: 0.98 }
                  }
                  transition={{ duration: 0.18, ease: [0.22, 1, 0.36, 1] }}
                >
                  <AskRow item={item} />
                </motion.div>
              ))}
            </AnimatePresence>
          </div>
        )}
      </div>
    </section>
  );
}

export function WriteTriggerRow({ item }: { item: WriteTrigger }) {
  const { approveWriteTrigger, denyWriteTrigger, selectThread } = useStore();
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
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
    <div className="overflow-hidden rounded-[var(--radius-lg)] border border-approval/40 bg-surface">
      <div className="flex items-center gap-2 px-3.5 pt-3 text-[12px]">
        <GitBranch size={13} className="shrink-0 text-approval" />
        <span className="text-ink-faint">{t("needs.wantsToWrite")}</span>
        <span className="font-mono font-medium text-ink">{item.repo_name}</span>
        {context && (
          <button
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
      <div className="flex flex-wrap items-center gap-2 border-t border-border bg-bg/40 px-3.5 py-2.5">
        <Button
          variant="primary"
          disabled={busy}
          title={t("needs.approveRunTitle")}
          onClick={() => void act(() => approveWriteTrigger(item))}
        >
          <Check size={13} />
          {t("needs.approveRun")}
        </Button>
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
  return (
    <div className="overflow-hidden rounded-[var(--radius-lg)] border border-approval/40 bg-surface">
      <div className="flex items-center gap-2 px-3.5 pt-3 text-[12px]">
        <ShieldQuestion size={13} className="shrink-0 text-approval" />
        <span className="font-medium capitalize text-ink">{ask.tool}</span>
        <span className="text-ink-faint">{t("needs.wantsPermission")}</span>
        <span className="ml-auto whitespace-nowrap text-ink-faint tabular-nums">
          {ago(ask.ts, t)}
        </span>
      </div>
      {context && (
        <button
          onClick={() => void selectThread(ask.thread)}
          title={t("needs.openDirection")}
          className="group flex max-w-full items-center gap-1.5 px-3.5 pt-1.5 text-[11px] text-ink-faint transition-colors hover:text-ink"
        >
          <Layers size={11} className="shrink-0" />
          <span className="truncate">{context}</span>
          <ArrowUpRight size={11} className="shrink-0 opacity-0 transition-opacity group-hover:opacity-100" />
        </button>
      )}
      <p className="truncate px-3.5 pb-1 pt-1.5 font-mono text-[13px] text-ink" title={ask.detail}>
        {ask.summary}
      </p>
      <div className="flex flex-wrap items-center gap-2 border-t border-border bg-bg/40 px-3.5 py-2.5">
        <Button
          variant="primary"
          title={t("needs.allowTitle")}
          onClick={() => void answerPermission(ask.id, "allow")}
        >
          <Check size={13} />
          {t("common.allow")}
        </Button>
        <Button
          variant="ghost"
          title={t("needs.alwaysTitle")}
          onClick={() => void answerPermission(ask.id, "always")}
        >
          <CheckCheck size={13} />
          {t("needs.always")}
        </Button>
        <Button
          variant="ghost"
          title={t("needs.fullAccessTitle")}
          onClick={() => void answerPermission(ask.id, "full")}
        >
          <ShieldCheck size={13} />
          {t("needs.fullAccess")}
        </Button>
        <Button
          variant="ghost"
          className="ml-auto"
          title={t("needs.denyTitle")}
          onClick={() => void answerPermission(ask.id, "deny")}
        >
          <X size={13} />
          {t("common.deny")}
        </Button>
      </div>
    </div>
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
    <div className="overflow-hidden rounded-[var(--radius-lg)] border border-border bg-surface">
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

      <form
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
        className="flex gap-2 border-t border-border bg-bg/40 px-3.5 py-2.5"
      >
        <Input
          autoFocus
          placeholder={t("needs.answerPlaceholder", { name: item.direction_name })}
          value={text}
          onChange={(e) => setText(e.currentTarget.value)}
        />
        <Button type="submit" variant="primary" size="icon" disabled={!text.trim() || busy}>
          <Send size={14} />
        </Button>
      </form>
    </div>
  );
}

function EmptyNeeds() {
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

/** Localized relative time. */
function ago(ts: number, t: TFunction): string {
  const s = Math.max(0, Math.floor(Date.now() / 1000) - ts);
  if (s < 60) return t("time.justNow");
  const m = Math.floor(s / 60);
  if (m < 60) return t("time.mAgo", { n: m });
  const h = Math.floor(m / 60);
  if (h < 24) return t("time.hAgo", { n: h });
  return t("time.dAgo", { n: Math.floor(h / 24) });
}
