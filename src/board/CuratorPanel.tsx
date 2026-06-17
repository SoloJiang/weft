import { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { PanelRightClose, Route } from "lucide-react";
import { useStore } from "../state/store";
import { LeadTab } from "../session/LeadTab";

/**
 * The dependency curator as a docked, collapsible panel inside the Repo Map.
 * Hosts the curator lead chat embedded (no navigation); calibrating an edge here
 * emits repo-graph-updated, so the graph beside it refreshes live. Mounted by
 * RepoMapView only when the workspace has >=2 profiled repos.
 */
export function CuratorPanel() {
  const {
    curatorThreadId,
    ensureCuratorThread,
    curatorPanelOpen,
    setCuratorPanelOpen,
    curatorPanelWidth,
    setCuratorPanelWidth,
  } = useStore();
  const { t } = useTranslation();

  // Lazily create/resolve the curator thread the first time the panel is open.
  useEffect(() => {
    if (curatorPanelOpen && curatorThreadId == null) void ensureCuratorThread();
  }, [curatorPanelOpen, curatorThreadId, ensureCuratorThread]);

  if (!curatorPanelOpen) {
    return (
      <button
        type="button"
        onClick={() => setCuratorPanelOpen(true)}
        aria-label={t("repomap.expandCurator")}
        title={t("repomap.expandCurator")}
        className="my-4 mr-4 flex w-10 shrink-0 flex-col items-center gap-2 rounded-[var(--radius-lg)] border border-border bg-surface pt-3 text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
      >
        <Route size={15} className="text-brand" />
        <span className="text-[11px] text-ink-muted [writing-mode:vertical-rl]">
          {t("repomap.curatorTitle")}
        </span>
      </button>
    );
  }

  return (
    <div
      className="relative my-4 mr-4 flex shrink-0 flex-col overflow-hidden rounded-[var(--radius-lg)] border border-border bg-surface"
      style={{ width: curatorPanelWidth }}
    >
      <ResizeEdge width={curatorPanelWidth} onResize={setCuratorPanelWidth} />
      <header className="flex items-center gap-2 border-b border-border px-3 py-2.5">
        <span className="grid h-6 w-6 shrink-0 place-items-center rounded-[var(--radius-md)] bg-brand-ghost">
          <Route size={13} className="text-brand" />
        </span>
        <div className="min-w-0 flex-1">
          <div className="truncate text-[13px] font-semibold text-ink">
            {t("repomap.curatorTitle")}
          </div>
          <div className="truncate text-[11px] text-ink-faint">{t("repomap.curatorSubtitle")}</div>
        </div>
        <button
          onClick={() => setCuratorPanelOpen(false)}
          aria-label={t("repomap.collapseCurator")}
          title={t("repomap.collapseCurator")}
          className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
        >
          <PanelRightClose size={14} />
        </button>
      </header>
      <div className="flex min-h-0 flex-1 flex-col">
        {curatorThreadId != null ? (
          <LeadTab threadId={curatorThreadId} compact onReview={() => {}} />
        ) : (
          <div className="flex h-full items-center justify-center text-[12px] text-ink-faint">
            {t("repomap.curatorLoading")}
          </div>
        )}
      </div>
    </div>
  );
}

/** A hit-area on the panel's left border to drag-resize its width. The panel is
 *  docked on the right, so dragging left widens it (delta inverted). */
function ResizeEdge({ width, onResize }: { width: number; onResize: (w: number) => void }) {
  const drag = useRef<{ x: number; w: number } | null>(null);
  return (
    <div
      onPointerDown={(e) => {
        drag.current = { x: e.clientX, w: width };
        e.currentTarget.setPointerCapture(e.pointerId);
      }}
      onPointerMove={(e) => {
        if (!drag.current) return;
        onResize(drag.current.w + (drag.current.x - e.clientX));
      }}
      onPointerUp={(e) => {
        drag.current = null;
        try {
          e.currentTarget.releasePointerCapture(e.pointerId);
        } catch {
          /* ignore */
        }
      }}
      className="absolute left-0 top-0 z-10 h-full w-1 cursor-col-resize hover:bg-brand/40"
      style={{ touchAction: "none" }}
    />
  );
}
