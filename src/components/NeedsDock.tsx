import { GitBranch, HelpCircle, Layers, ShieldQuestion } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { PermissionAsk, WriteTrigger, NeedItem } from "../lib/types";
import { cn } from "../lib/cn";
import { useStore } from "../state/store";
import { ToolIcon, toolFullName } from "./ToolIcon";

type DockItem =
  | { kind: "write"; item: WriteTrigger }
  | { kind: "permission"; item: PermissionAsk }
  | { kind: "question"; item: NeedItem };

export function NeedsDock() {
  const { needs, asks, writeTriggers, openNeeds } = useStore();
  const { t } = useTranslation();
  const total = needs.length + asks.length + writeTriggers.length;
  const top: DockItem | null =
    writeTriggers[0] != null
      ? { kind: "write", item: writeTriggers[0] }
      : asks[0] != null
        ? { kind: "permission", item: asks[0] }
        : needs[0] != null
          ? { kind: "question", item: needs[0] }
          : null;

  // Nothing needs the human → no banner at all: "flowing automatically" is the
  // default state, not an announcement worth a permanent strip on every screen.
  if (total === 0) return null;

  return (
    <button
      type="button"
      onClick={openNeeds}
      className="group flex h-10 shrink-0 items-center gap-2 border-b border-waiting/30 bg-waiting/10 px-5 text-left text-[12px] transition-colors hover:bg-waiting/15"
    >
      <span className="grid h-5 min-w-5 place-items-center rounded-full bg-waiting text-[11px] font-semibold tabular-nums text-bg">
        {total}
      </span>
      <span className="font-semibold text-waiting">{t("needs.title")}</span>
      {top && <DockSummary top={top} />}
      <span className="ml-auto text-[11.5px] text-ink-faint">
        {t("needs.openQueue")}
      </span>
    </button>
  );
}

function DockSummary({ top }: { top: DockItem }) {
  const { t } = useTranslation();
  if (top.kind === "write") {
    const item = top.item;
    return (
      <span className="flex min-w-0 items-center gap-1.5 text-ink-muted">
        <GitBranch size={13} className="shrink-0 text-approval" />
        <span className="truncate">{item.thread_title}</span>
        <span className="text-ink-faint">·</span>
        <span className="truncate font-mono text-ink">{item.repo_name}</span>
      </span>
    );
  }

  if (top.kind === "permission") {
    const item = top.item;
    return (
      <span className="flex min-w-0 items-center gap-1.5 text-ink-muted">
        <ShieldQuestion size={13} className="shrink-0 text-approval" />
        <ToolIcon tool={item.tool} size={13} />
        <span className="text-ink">{toolFullName(item.tool)}</span>
        <span>{t("needs.wantsPermission")}</span>
        <ContextText text={[item.thread_title, item.dir_name].filter(Boolean).join(" · ")} />
      </span>
    );
  }

  const item = top.item;
  return (
    <span className="flex min-w-0 items-center gap-1.5 text-ink-muted">
      <HelpCircle size={13} className="shrink-0 text-waiting" />
      <span>{t("needs.question")}</span>
      <ContextText text={[item.thread_title, item.direction_name].filter(Boolean).join(" · ")} />
    </span>
  );
}

function ContextText({ text }: { text: string }) {
  if (!text) return null;
  return (
    <>
      <span className="text-ink-faint">·</span>
      <span className={cn("flex min-w-0 items-center gap-1 truncate")}>
        <Layers size={11} className="shrink-0 text-ink-faint" />
        <span className="truncate">{text}</span>
      </span>
    </>
  );
}
