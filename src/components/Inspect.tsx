import * as DM from "@radix-ui/react-dropdown-menu";
import { useTranslation } from "react-i18next";
import {
  Copy,
  FolderOpen,
  MoreHorizontal,
  SquareTerminal,
} from "lucide-react";
import { api } from "../lib/api";
import { cn } from "../lib/cn";

/**
 * The escape hatch (architecture §4.7). The product hides plumbing — worktree
 * paths, branches, native session ids — but Inspect always offers the real way
 * out: open the isolated working copy in a terminal or the file manager, copy
 * its path, and read the underlying git/session identifiers.
 */
export function Inspect({
  path,
  branch,
  nativeId,
  className,
  size = 14,
}: {
  path: string;
  branch?: string;
  nativeId?: string | null;
  className?: string;
  size?: number;
}) {
  const { t } = useTranslation();
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
          <Item icon={<SquareTerminal size={13} />} onSelect={() => void api.openTerminal(path)}>
            {t("inspect.openTerminal")}
          </Item>
          <Item icon={<FolderOpen size={13} />} onSelect={() => void api.revealPath(path)}>
            {t("inspect.reveal")}
          </Item>
          <Item
            icon={<Copy size={13} />}
            onSelect={() => void navigator.clipboard?.writeText(path)}
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
  onSelect: () => void;
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
