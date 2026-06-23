import { useEffect, useRef, useState, type ReactNode } from "react";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { X, RefreshCw, Check, AlertTriangle } from "lucide-react";
import { useStore, type AnalysisOutcome } from "../state/store";
import { LeadTab } from "../session/LeadTab";
import { RepoDetailContent } from "./RepoGraph";
import { Markdown } from "../components/Markdown";
import { api } from "../lib/api";
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

type CuratorView = "chat" | "map";

/** The curator surface, kept mounted across switches; `hidden` when inactive. */
function CuratorBody({ active, threadId }: { active: boolean; threadId: number | null }) {
  const { t } = useTranslation();
  const { activeWorkspaceId, reanalyzeDeps, analyzing, analysisOutcome } = useStore();
  const [view, setView] = useState<CuratorView>("chat");
  const [mapDoc, setMapDoc] = useState<string | null | undefined>(undefined);

  useEffect(() => {
    // Gate on `active` too: this panel stays mounted while hidden behind the detail
    // surface, and detail-side mutations (profile edit, delete) clear the persisted
    // doc while we're hidden. Re-running when the panel becomes active again
    // refetches, so we never render markdown the backend already invalidated.
    if (!active || view !== "map" || activeWorkspaceId == null) return;
    const ws = activeWorkspaceId;
    let cancelled = false;
    // Single guarded fetch used by the initial load, the graph-update re-fetch, and
    // the re-show refetch. `mapDoc` is kept in the mounted curator panel, so without
    // the `cancelled` + captured-`ws` guard a late response could repopulate the
    // panel with another workspace's markdown after a switch. A failed fetch falls
    // back to empty.
    const load = () => {
      api
        .getRepoMapDoc(ws)
        .then((doc) => {
          if (!cancelled) setMapDoc(doc);
        })
        .catch(() => {
          if (!cancelled) setMapDoc(null);
        });
    };
    // Reset to the loading state immediately so a switch/re-show never shows a stale map.
    setMapDoc(undefined);
    load();
    // Re-fetch when the backend signals a graph update while map is open.
    const unlistenP = listen<number>("repo-graph-updated", (e) => {
      if (e.payload === ws) load();
    });
    return () => {
      cancelled = true;
      void unlistenP.then((f) => f());
    };
  }, [active, view, activeWorkspaceId]);

  function renderMapBody() {
    if (mapDoc === undefined) {
      return (
        <div className="flex h-full items-center justify-center text-[12px] text-ink-faint">
          {t("repomap.curatorLoading")}
        </div>
      );
    }
    if (mapDoc === null) {
      return (
        <div className="flex h-full flex-col items-center justify-center gap-3 px-4 text-center">
          <p className="text-[12px] text-ink-faint">{t("repomap.mapEmpty")}</p>
          <button
            onClick={() => void reanalyzeDeps()}
            disabled={analyzing}
            className="flex items-center gap-1.5 rounded-[var(--radius-md)] bg-brand-ghost px-3 py-1.5 text-[11.5px] font-medium text-brand transition-colors hover:bg-brand/20 disabled:opacity-50"
          >
            <RefreshCw size={12} className={analyzing ? "animate-spin" : ""} />
            {t("repomap.reanalyze")}
          </button>
        </div>
      );
    }
    return (
      <div className="min-h-0 min-w-0 flex-1 overflow-y-auto px-4 py-3">
        <div className="mb-3 flex justify-end">
          <button
            onClick={() => void reanalyzeDeps()}
            disabled={analyzing}
            title={t("repomap.reanalyze")}
            className="flex items-center gap-1 rounded-[var(--radius-md)] px-2 py-1 text-[11px] text-ink-faint transition-colors hover:bg-raised hover:text-ink disabled:opacity-50"
          >
            <RefreshCw size={11} className={analyzing ? "animate-spin" : ""} />
            {t("repomap.reanalyze")}
          </button>
        </div>
        <Markdown text={mapDoc} />
      </div>
    );
  }

  return (
    <div className={cn("min-h-0 flex-1 flex-col", active ? "flex" : "hidden")}>
      {/* Non-conversational analysis status (NOT a chat row): the curator is the home
          of analysis, so its progress/result shows here, above both sub-tabs. */}
      <AnalysisStatusStrip analyzing={analyzing} outcome={analysisOutcome} />
      {/* chat / map segmented toggle */}
      <div className="flex shrink-0 gap-0.5 border-b border-border px-3 py-1.5">
        <button
          onClick={() => setView("chat")}
          className={cn(
            "rounded-[var(--radius-sm)] px-2.5 py-1 text-[11.5px] font-medium transition-colors",
            view === "chat"
              ? "bg-raised text-ink"
              : "text-ink-faint hover:bg-raised/60 hover:text-ink",
          )}
        >
          {t("repomap.chatTab")}
        </button>
        <button
          onClick={() => setView("map")}
          className={cn(
            "rounded-[var(--radius-sm)] px-2.5 py-1 text-[11.5px] font-medium transition-colors",
            view === "map"
              ? "bg-raised text-ink"
              : "text-ink-faint hover:bg-raised/60 hover:text-ink",
          )}
        >
          {t("repomap.mapTab")}
        </button>
      </div>

      {/* chat view: keep LeadTab mounted so scroll/draft survive toggling */}
      <div className={cn("min-h-0 flex-1 flex-col", view === "chat" ? "flex" : "hidden")}>
        {threadId != null ? (
          <LeadTab
            threadId={threadId}
            compact
            composePlaceholder={t("repomap.curatorCompose")}
            emptyState={
              <div className="flex flex-1 items-center justify-center px-6 text-center">
                <p className="max-w-[420px] text-[12px] leading-relaxed text-ink-faint">
                  {t("repomap.curatorEmpty")}
                </p>
              </div>
            }
            onReview={() => {}}
          />
        ) : (
          <div className="flex h-full items-center justify-center text-[12px] text-ink-faint">
            {t("repomap.curatorLoading")}
          </div>
        )}
      </div>

      {/* map view */}
      {view === "map" && (
        <div className="min-h-0 flex-1 flex flex-col">
          {renderMapBody()}
        </div>
      )}
    </div>
  );
}

type AnalysisStatusKind = "running" | "done" | "failed";

/** One discriminated status from the run-state + last outcome (null = show nothing). */
function analysisStatusKind(
  analyzing: boolean,
  outcome: AnalysisOutcome | null,
): AnalysisStatusKind | null {
  if (analyzing) return "running";
  if (!outcome) return null;
  return outcome.state;
}

/** A thin, non-conversational status line for the dependency-analysis pass — the
 *  curator panel's record of analysis (deliberately NOT a chat message). Coarse by
 *  design: "ran" vs "failed to run"; per-repo detail lives on the graph nodes. */
function AnalysisStatusStrip({
  analyzing,
  outcome,
}: {
  analyzing: boolean;
  outcome: AnalysisOutcome | null;
}) {
  const { t } = useTranslation();
  const kind = analysisStatusKind(analyzing, outcome);
  if (kind == null) return null;
  const view: Record<AnalysisStatusKind, { icon: ReactNode; text: string; tone: string }> = {
    running: {
      icon: <RefreshCw size={11} className="shrink-0 animate-spin text-brand" />,
      text: t("repomap.analysisRunning"),
      tone: "text-ink-muted",
    },
    done: {
      icon: <Check size={12} className="shrink-0 text-running" />,
      text: t("repomap.analysisDone"),
      tone: "text-ink-muted",
    },
    failed: {
      icon: <AlertTriangle size={11} className="shrink-0 text-danger" />,
      text: t("repomap.analysisFailed"),
      tone: "text-danger",
    },
  };
  const v = view[kind];
  return (
    <div className="flex shrink-0 items-center gap-1.5 border-b border-border bg-surface px-3 py-1.5 text-[11px]">
      {v.icon}
      <span className={cn("min-w-0 truncate", v.tone)}>{v.text}</span>
    </div>
  );
}
