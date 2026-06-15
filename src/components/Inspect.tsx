import { useState } from "react";
import * as DM from "@radix-ui/react-dropdown-menu";
import { useTranslation } from "react-i18next";
import { Check, Copy, ExternalLink, FolderOpen, MoreHorizontal, Terminal } from "lucide-react";
import { api } from "../lib/api";
import { toast } from "./Toast";
import { appLink, resumeCommand } from "../lib/resume";
import { ToolIcon } from "./ToolIcon";
import { cn } from "../lib/cn";

/**
 * The per-session "…" menu (escape hatch, §4.7 + resume §5.6). Leads with the
 * way to pick the session back up in your own tools — copy the `cd … && <tool>
 * resume <id>` command, or jump to it in the Codex app — then Reveal / Copy
 * path. No "open terminal": an empty shell at the worktree doesn't resume.
 */
export function Inspect({
  path,
  branch,
  nativeId,
  tool,
  command,
  className,
  size = 14,
}: {
  path: string;
  branch?: string;
  nativeId?: string | null;
  tool?: string;
  /** Effective binary for the resume command (configured alias, else `tool`). */
  command?: string;
  className?: string;
  size?: number;
}) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const link = tool && nativeId ? appLink(tool, nativeId) : null;

  async function copyResume() {
    if (!tool || !nativeId) return;
    try {
      await navigator.clipboard?.writeText(resumeCommand(tool, path, nativeId, command));
      setCopied(true);
      setTimeout(() => setCopied(false), 1400);
    } catch {
      /* ignore */
    }
  }

  return (
    <DM.Root>
      <DM.Trigger
        aria-label="Inspect"
        title={t("inspect.label")}
        className={cn(
          "grid place-items-center rounded text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink",
          className,
        )}
        onClick={(e) => e.stopPropagation()}
      >
        <MoreHorizontal size={size} />
      </DM.Trigger>
      <DM.Portal>
        <DM.Content
          align="end"
          sideOffset={4}
          onClick={(e) => e.stopPropagation()}
          className="weft-pop z-[60] w-64 rounded-[var(--radius-md)] border border-border bg-raised p-1 shadow-[0_8px_24px_-8px_rgba(0,0,0,0.5)]"
        >
          {nativeId && tool && (
            <>
              <Item
                icon={copied ? <Check size={13} className="text-running" /> : <Terminal size={13} />}
                onSelect={(e) => {
                  e.preventDefault();
                  void copyResume();
                }}
              >
                {copied ? t("resume.copied") : t("resume.copyCommand")}
              </Item>
              {link && (
                <Item icon={<ToolIcon tool="codex" size={13} />} onSelect={() => void api.openUrl(link)}>
                  {t("resume.openInCodex")}
                  <ExternalLink size={11} className="ml-auto text-ink-faint" />
                </Item>
              )}
              <DM.Separator className="my-1 h-px bg-border" />
            </>
          )}

          <Item icon={<FolderOpen size={13} />} onSelect={() => void api.revealPath(path)}>
            {t("inspect.reveal")}
          </Item>
          <Item
            icon={<Copy size={13} />}
            onSelect={() => {
              void navigator.clipboard?.writeText(path);
              toast(t("resume.copied"));
            }}
          >
            {t("inspect.copyPath")}
          </Item>

          {(branch || nativeId) && (
            <>
              <DM.Separator className="my-1 h-px bg-border" />
              <div className="flex flex-col gap-1.5 px-2 py-1.5">
                {branch && <MetaRow label={t("inspect.branchLabel")} value={branch} />}
                {nativeId && (
                  <MetaRow label={t("inspect.sessionLabel")} value={nativeId.slice(0, 12)} />
                )}
              </div>
            </>
          )}
        </DM.Content>
      </DM.Portal>
    </DM.Root>
  );
}

function Item({
  icon,
  children,
  onSelect,
}: {
  icon: React.ReactNode;
  children: React.ReactNode;
  onSelect: (e: Event) => void;
}) {
  return (
    <DM.Item
      onSelect={onSelect}
      className="flex cursor-pointer items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-[12px] text-ink-muted outline-none data-[highlighted]:bg-brand-ghost data-[highlighted]:text-ink"
    >
      <span className="text-ink-faint">{icon}</span>
      {children}
    </DM.Item>
  );
}

function MetaRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline gap-2 text-[11px]">
      <span className="w-12 shrink-0 text-ink-faint">{label}</span>
      <span className="truncate font-mono text-ink-muted" title={value}>
        {value}
      </span>
    </div>
  );
}
