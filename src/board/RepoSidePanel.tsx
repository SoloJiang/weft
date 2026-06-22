import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { X } from "lucide-react";
import { useStore } from "../state/store";
import { LeadTab } from "../session/LeadTab";
import { RepoDetailContent } from "./RepoGraph";
import { cn } from "../lib/cn";

// Per-surface remembered width (localStorage, like DiffPanel/FileTreePanel — not
// per-workspace store state). Detail is profile-narrow; the curator chat is wider.
const WIDTH = {
  detail: { key: "weft-repodetail-w", def: 360, min: 280, max: 520 },
  curator: { key: "weft-curator-w", def: 420, min: 320, max: 620 },
} as const;
type Surface = keyof typeof WIDTH;
const clampW = (s: Surface, n: number) => Math.max(WIDTH[s].min, Math.min(WIDTH[s].max, n));

/**
 * The Repos view's right side panel — one mutually-exclusive slot, no tabs.
 * Detail (RepoDetailContent) opens by clicking a repo card; the dependency
 * curator (LeadTab) opens from the top-bar button. Modeled on DiffPanel: an
 * animated-width flex column that pushes the graph narrower (not an overlay),
 * with a left-edge drag-resize and per-surface remembered width. The curator
 * chat is mounted lazily on first open and kept alive after that.
 */
export function RepoSidePanel() {
  const { repoDrawerOpen, repoDrawerTab, selectedRepoId, closeRepoDrawer, curatorThreadId, ensureCuratorThread } =
    useStore();
  const { t } = useTranslation();
  const surface = repoDrawerTab as Surface;

  const [widths, setWidths] = useState(() => ({
    detail: clampW("detail", Number(localStorage.getItem(WIDTH.detail.key)) || WIDTH.detail.def),
    curator: clampW("curator", Number(localStorage.getItem(WIDTH.curator.key)) || WIDTH.curator.def),
  }));
  const w = widths[surface];
  const [dragging, setDragging] = useState(false);
  const drag = useRef<{ x: number; w: number } | null>(null);
  const setW = (n: number) => {
    const cw = clampW(surface, n);
    setWidths((prev) => ({ ...prev, [surface]: cw }));
    localStorage.setItem(WIDTH[surface].key, String(cw));
  };

  // Lazily create the curator thread on first open of the curator surface, and
  // keep LeadTab mounted thereafter (preserve chat scroll / draft across switches).
  const [curatorMounted, setCuratorMounted] = useState(false);
  useEffect(() => {
    if (repoDrawerOpen && repoDrawerTab === "curator") {
      setCuratorMounted(true);
      if (curatorThreadId == null) void ensureCuratorThread();
    }
  }, [repoDrawerOpen, repoDrawerTab, curatorThreadId, ensureCuratorThread]);

  // Esc closes the panel — but defer to an editable field (inline summary editor,
  // chat composer) or a nested Radix modal (`.weft-overlay`, the delete-repo
  // confirm) that owns Escape, so a field cancel / modal dismiss doesn't nuke the
  // whole panel. (Carried over from the merged RepoDrawer.)
  useEffect(() => {
    if (!repoDrawerOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || e.defaultPrevented) return;
      if (document.querySelector(".weft-overlay")) return;
      const el = e.target as HTMLElement | null;
      if (el && (el.isContentEditable || el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.tagName === "SELECT")) {
        return;
      }
      closeRepoDrawer();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [repoDrawerOpen, closeRepoDrawer]);

  const title = surface === "detail" ? t("repomap.detailTab") : t("repomap.curatorTitle");
  const detailActive = repoDrawerOpen && repoDrawerTab === "detail";
  const curatorActive = repoDrawerOpen && repoDrawerTab === "curator";

  return (
    <div
      style={{ width: repoDrawerOpen ? w : 0 }}
      className={cn(
        "relative flex shrink-0 overflow-hidden",
        !dragging && "transition-[width] duration-200 ease-out motion-reduce:transition-none",
      )}
    >
      {/* left-edge drag handle (panel is on the right, so dragging left widens it) */}
      <div
        onPointerDown={(e) => {
          e.preventDefault();
          drag.current = { x: e.clientX, w };
          setDragging(true);
          e.currentTarget.setPointerCapture(e.pointerId);
        }}
        onPointerMove={(e) => {
          if (!drag.current) return;
          setW(drag.current.w + (drag.current.x - e.clientX));
        }}
        onPointerUp={(e) => {
          drag.current = null;
          setDragging(false);
          try {
            e.currentTarget.releasePointerCapture(e.pointerId);
          } catch {
            /* ignore */
          }
        }}
        className={cn(
          "absolute left-0 top-0 z-10 h-full w-1.5 cursor-col-resize transition-colors",
          dragging ? "bg-brand/40" : "hover:bg-brand/30",
        )}
        style={{ touchAction: "none" }}
      />
      {/* fixed-width inner so content doesn't reflow while the column animates */}
      <aside style={{ width: w }} className="flex h-full shrink-0 flex-col border-l border-border bg-bg">
        <header className="flex items-center gap-2 border-b border-border px-4 py-2.5">
          <span className="text-[12px] font-semibold text-ink">{title}</span>
          <button
            onClick={closeRepoDrawer}
            aria-label={t("common.close")}
            title={t("common.close")}
            className="ml-auto grid h-7 w-7 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
          >
            <X size={15} />
          </button>
        </header>
        {/* Detail mounts on demand; the curator stays mounted once opened (hidden
            when inactive) to preserve its chat state. */}
        {detailActive && <RepoDetailContent repoId={selectedRepoId} />}
        {curatorMounted && <CuratorBody active={curatorActive} threadId={curatorThreadId} />}
      </aside>
    </div>
  );
}

/** The curator surface, kept mounted across switches; `hidden` when inactive. */
function CuratorBody({ active, threadId }: { active: boolean; threadId: number | null }) {
  const { t } = useTranslation();
  return (
    <div className={cn("min-h-0 flex-1 flex-col", active ? "flex" : "hidden")}>
      {threadId != null ? (
        <LeadTab threadId={threadId} compact onReview={() => {}} />
      ) : (
        <div className="flex h-full items-center justify-center text-[12px] text-ink-faint">
          {t("repomap.curatorLoading")}
        </div>
      )}
    </div>
  );
}
