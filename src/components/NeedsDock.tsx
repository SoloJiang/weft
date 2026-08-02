import {
  AlertTriangle,
  ClipboardCheck,
  GitBranch,
  HelpCircle,
  Layers,
  ShieldQuestion,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import type { ReactNode } from "react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import type { AttentionItem } from "../lib/types";
import { cn } from "../lib/cn";
import { pendingNeedsCount, useStore } from "../state/store";
import { needsBarMotion } from "../lib/motion";

/** A quiet router into the canonical action queue. */
export function NeedsDock() {
  const { attentionItems, openNeeds } = useStore();
  const { t } = useTranslation();
  const reduce = useReducedMotion();
  const total = pendingNeedsCount(attentionItems);
  const top = attentionItems[0] ?? null;

  return (
    <AnimatePresence initial={false}>
      {total > 0 && (
        <motion.div key="needs-dock" {...needsBarMotion(!!reduce)} className="shrink-0 overflow-hidden">
          <button
            type="button"
            onClick={openNeeds}
            className="group flex h-10 w-full items-center gap-2 border-b border-waiting/30 bg-waiting/10 px-5 text-left text-[12px] transition-colors hover:bg-waiting/15"
          >
            <span className="grid h-5 min-w-5 place-items-center rounded-full bg-waiting text-[11px] font-semibold tabular-nums text-bg">
              {total}
            </span>
            <span className="font-semibold text-waiting">{t("needs.title")}</span>
            {top && <DockSummary item={top} />}
            <span className="ml-auto text-[11.5px] text-ink-faint transition-colors group-hover:text-ink">
              {t("needs.openQueue")}
            </span>
          </button>
        </motion.div>
      )}
    </AnimatePresence>
  );
}

function DockSummary({ item }: { item: AttentionItem }) {
  const { t } = useTranslation();
  switch (item.kind) {
    case "permission":
      return (
        <Summary icon={<ShieldQuestion size={13} className="text-approval" />} label={`${item.ask.tool} ${t("needs.wantsPermission")}`} context={[item.ask.thread_title, item.ask.dir_name]} />
      );
    case "question":
      return <Summary icon={<HelpCircle size={13} className="text-waiting" />} label={t("needs.question")} context={[item.thread_title, item.direction_name]} />;
    case "plan_approval":
      return <Summary icon={<ClipboardCheck size={13} className="text-approval" />} label={t("needs.planApproval")} context={[item.thread_title, item.title]} />;
    case "scope_approval":
      return <Summary icon={<Layers size={13} className="text-approval" />} label={t("needs.scopeApproval")} context={[item.thread_title]} />;
    case "repo_action":
      return <Summary icon={<GitBranch size={13} className="text-approval" />} label={t("needs.repoAction")} context={[item.thread_title, item.title]} />;
    case "pr_tracking_retry":
      return <Summary icon={<AlertTriangle size={13} className="text-danger" />} label={t("needs.prTrackingRetry")} context={[item.thread_title, item.direction_name]} />;
  }
}

function Summary({
  icon,
  label,
  context,
}: {
  icon: ReactNode;
  label: string;
  context: Array<string | undefined>;
}) {
  const text = context.filter(Boolean).join(" · ");
  return (
    <span className="flex min-w-0 items-center gap-1.5 text-ink-muted">
      <span className="shrink-0">{icon}</span>
      <span className="shrink-0 text-ink">{label}</span>
      {text && (
        <>
          <span className="text-ink-faint">·</span>
          <span className={cn("flex min-w-0 items-center gap-1 truncate")}>
            <Layers size={11} className="shrink-0 text-ink-faint" />
            <span className="truncate">{text}</span>
          </span>
        </>
      )}
    </span>
  );
}
