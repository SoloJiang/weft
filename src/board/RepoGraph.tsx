import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  AppWindow,
  Boxes,
  CircleDashed,
  GitBranch,
  Layers,
  Loader2,
  Maximize2,
  MessagesSquare,
  Minus,
  Network,
  PanelRightClose,
  PanelRightOpen,
  Pencil,
  Plus,
  RefreshCw,
  Server,
  Trash2,
  type LucideProps,
} from "lucide-react";
import type { ComponentType } from "react";
import { useStore } from "../state/store";
import type { RepoComponent, RepoEdge, RepoProfile } from "../lib/types";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { Button } from "../components/ui/Button";
import { cn } from "../lib/cn";

/** The three architectural tiers laid out left→right, plus the catch-all "other"
 *  band that holds unclassified / still-analyzing repos. */
const TIER_ORDER = ["frontend", "gateway", "backend"] as const;
const BANDS = ["frontend", "gateway", "backend", "other"] as const;
type Band = (typeof BANDS)[number];

const TIER_ICON: Record<string, ComponentType<LucideProps>> = {
  frontend: AppWindow,
  gateway: Network,
  backend: Server,
  other: CircleDashed,
};

/** The relation kinds an edge can carry, for the filter toggles. */
const KINDS = ["lib", "http", "grpc", "queue", "infra"] as const;

const bandOf = (tier: string): Band =>
  (TIER_ORDER as readonly string[]).includes(tier) ? (tier as Band) : "other";

/** Whether the user has pinned the summary (mirrors the backend `source` flags:
 *  "user" = both fields owned, "user_summary" = just the summary). */
const ownsSummary = (source: string): boolean => source === "user" || source === "user_summary";

type ViewMode = "overview" | "expanded";

const NODE_W = 248;
const NODE_H = 108;
const COL_GAP = 82;
const ROW_GAP = 18;
const PAD = 18;
// Expanded-container metrics (a repo's monorepo components grouped by tier).
const EXP_HEADER_H = 60;
const EXP_TIER_SUB_H = 20;
const EXP_COMP_H = 40;
const MIN_Z = 0.35;
const MAX_Z = 2.5;
const clampZ = (z: number) => Math.min(MAX_Z, Math.max(MIN_Z, z));

/** Group a repo's components into tier bands, in tier order then "other". */
function groupComponents(components: RepoComponent[]): [Band, RepoComponent[]][] {
  const out: [Band, RepoComponent[]][] = [];
  for (const band of BANDS) {
    const inBand = components.filter((c) => bandOf(c.tier) === band);
    if (inBand.length > 0) out.push([band, inBand]);
  }
  return out;
}

/** The drawn height of a repo node: a fixed card, or a taller container when
 *  expanded and the repo has monorepo components. */
function nodeHeight(p: RepoProfile, mode: ViewMode): number {
  if (mode !== "expanded" || p.components.length === 0) return NODE_H;
  let h = EXP_HEADER_H + 8;
  for (const [, comps] of groupComponents(p.components)) {
    h += EXP_TIER_SUB_H + comps.length * EXP_COMP_H + 6;
  }
  return Math.max(h, NODE_H);
}

/**
 * The repo map as a pan/zoom canvas — the whole Repos surface. Nodes are laid
 * out in bands by architectural TIER (frontend → gateway → backend → other),
 * agent-classified. Switch to the expanded view to break monorepos into their
 * internal components, grouped by tier. Edges are agent-inferred cross-repo
 * relations and can be filtered by kind. Drag to pan, scroll/buttons to zoom.
 */
export function RepoGraph() {
  const { repoProfiles, repoEdges, reprofileRepo, reanalyzeDeps, openCuratorChat } = useStore();
  const [analyzing, setAnalyzing] = useState(false);
  const { t } = useTranslation();
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [profileOpen, setProfileOpen] = useState(true);
  const [mode, setMode] = useState<ViewMode>("overview");
  const [activeKinds, setActiveKinds] = useState<Set<string>>(() => new Set(KINDS));
  const seededSelection = useRef(false);

  const toggleKind = useCallback((kind: string) => {
    setActiveKinds((prev) => {
      const next = new Set(prev);
      if (next.has(kind)) next.delete(kind);
      else next.add(kind);
      return next;
    });
  }, []);

  // An edge is shown unless its kind has been filtered out (unknown kinds — e.g.
  // a legacy "dep" — always show so nothing silently vanishes).
  const visibleEdges = useMemo(
    () =>
      repoEdges.filter(
        (e) => !e.kind || !(KINDS as readonly string[]).includes(e.kind) || activeKinds.has(e.kind),
      ),
    [repoEdges, activeKinds],
  );

  // Seed the profile pane once so the repo map starts useful, while still
  // letting the user close the pane afterwards.
  useEffect(() => {
    if (repoProfiles.length === 0) {
      seededSelection.current = false;
      setSelectedId(null);
      return;
    }
    if (!seededSelection.current) {
      seededSelection.current = true;
      setSelectedId(repoProfiles[0].repo_id);
      return;
    }
    if (selectedId != null && !repoProfiles.some((p) => p.repo_id === selectedId)) {
      setSelectedId(repoProfiles[0].repo_id);
    }
  }, [repoProfiles, selectedId]);

  const layout = useMemo(() => {
    const dependents = (id: number) => repoEdges.filter((e) => e.to === id).length;
    // Bucket repos into tier bands; within a band, the most-depended-on first.
    const bands = new Map<Band, RepoProfile[]>();
    for (const p of repoProfiles) {
      const band = bandOf(p.tier);
      const arr = bands.get(band) ?? [];
      arr.push(p);
      bands.set(band, arr);
    }
    for (const arr of bands.values()) {
      arr.sort((a, b) => dependents(b.repo_id) - dependents(a.repo_id) || a.repo_name.localeCompare(b.repo_name));
    }
    const visibleBands = BANDS.filter((b) => (bands.get(b)?.length ?? 0) > 0);

    const bandHeight = (b: Band) => {
      const arr = bands.get(b) ?? [];
      return arr.reduce((sum, p) => sum + nodeHeight(p, mode) + ROW_GAP, 0) - ROW_GAP;
    };
    const maxH = Math.max(0, ...visibleBands.map(bandHeight));

    const pos = new Map<number, { x: number; y: number; w: number; h: number }>();
    visibleBands.forEach((b, col) => {
      const arr = bands.get(b) ?? [];
      const x = PAD + col * (NODE_W + COL_GAP);
      let y = PAD + (maxH - bandHeight(b)) / 2;
      for (const p of arr) {
        const h = nodeHeight(p, mode);
        pos.set(p.repo_id, { x, y, w: NODE_W, h });
        y += h + ROW_GAP;
      }
    });

    const width = Math.max(NODE_W + PAD * 2, PAD * 2 + visibleBands.length * NODE_W + Math.max(0, visibleBands.length - 1) * COL_GAP);
    const height = PAD * 2 + Math.max(NODE_H, maxH);
    return { pos, width, height, bands: visibleBands };
  }, [repoProfiles, repoEdges, mode]);

  const containerRef = useRef<HTMLDivElement>(null);
  const [zoom, setZoom] = useState(1);
  const [pan, setPan] = useState({ x: 0, y: 0 });
  const drag = useRef<{ x: number; y: number; px: number; py: number } | null>(null);

  const fit = useCallback(() => {
    const el = containerRef.current;
    if (!el) return;
    const cw = el.clientWidth;
    const ch = el.clientHeight;
    const z = clampZ(Math.min((cw - 56) / layout.width, (ch - 56) / layout.height, 1));
    setZoom(z);
    setPan({ x: (cw - layout.width * z) / 2, y: (ch - layout.height * z) / 2 });
  }, [layout.width, layout.height]);

  // fit on first paint + whenever the graph shape changes
  useLayoutEffect(() => {
    fit();
  }, [fit]);

  // zoom toward a point in container space
  const zoomAt = useCallback((cx: number, cy: number, factor: number) => {
    setZoom((z0) => {
      const nz = clampZ(z0 * factor);
      setPan((p) => ({ x: cx - ((cx - p.x) / z0) * nz, y: cy - ((cy - p.y) / z0) * nz }));
      return nz;
    });
  }, []);

  // non-passive wheel so we can preventDefault the page scroll
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const rect = el.getBoundingClientRect();
      const dy = Math.max(-50, Math.min(50, e.deltaY));
      zoomAt(e.clientX - rect.left, e.clientY - rect.top, Math.exp(-dy * 0.0045));
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, [zoomAt]);

  const zoomButton = (factor: number) => {
    const el = containerRef.current;
    if (!el) return;
    zoomAt(el.clientWidth / 2, el.clientHeight / 2, factor);
  };

  const onPointerDown = (e: React.PointerEvent) => {
    if ((e.target as HTMLElement).closest("[data-repo-node], [data-graph-controls]")) return;
    drag.current = { x: e.clientX, y: e.clientY, px: pan.x, py: pan.y };
    e.currentTarget.setPointerCapture(e.pointerId);
  };
  const onPointerMove = (e: React.PointerEvent) => {
    if (!drag.current) return;
    setPan({
      x: drag.current.px + (e.clientX - drag.current.x),
      y: drag.current.py + (e.clientY - drag.current.y),
    });
  };
  const endDrag = (e: React.PointerEvent) => {
    drag.current = null;
    try {
      e.currentTarget.releasePointerCapture(e.pointerId);
    } catch {
      /* ignore */
    }
  };

  const anyComponents = repoProfiles.some((p) => p.components.length > 0);

  return (
    <div className="flex h-full min-h-0 w-full gap-3 bg-bg p-4">
      <div
        ref={containerRef}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={endDrag}
        onPointerLeave={endDrag}
        className="relative min-w-0 flex-1 cursor-grab select-none overflow-hidden rounded-[var(--radius-lg)] border border-border bg-surface/35 [touch-action:none] active:cursor-grabbing"
      >
        {/* Tier band headers — fixed in canvas space so they track pan/zoom. */}
        <div
          className="pointer-events-none absolute left-0 top-0 origin-top-left"
          style={{ transform: `translate(${pan.x}px, ${pan.y}px) scale(${zoom})` }}
        >
          {layout.bands.map((b, col) => (
            <div
              key={b}
              className="absolute text-[10.5px] font-medium uppercase tracking-wide text-ink-faint"
              style={{ left: PAD + col * (NODE_W + COL_GAP), top: 0, width: NODE_W }}
            >
              {t(`repomap.tier_${b}`, b)}
            </div>
          ))}
        </div>

        <div
          className="absolute left-0 top-0 origin-top-left"
          style={{
            width: layout.width,
            height: layout.height,
            transform: `translate(${pan.x}px, ${pan.y}px) scale(${zoom})`,
          }}
        >
          <svg className="absolute inset-0" width={layout.width} height={layout.height} fill="none">
            <defs>
              <marker id="weft-arrow" viewBox="0 0 8 8" refX="6" refY="4" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
                <path d="M0 0 L8 4 L0 8 z" className="fill-border-strong" />
              </marker>
              <marker id="weft-arrow-active" viewBox="0 0 8 8" refX="6" refY="4" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
                <path d="M0 0 L8 4 L0 8 z" className="fill-brand" />
              </marker>
            </defs>
            {visibleEdges.map((e, i) => {
              const a = layout.pos.get(e.from);
              const b = layout.pos.get(e.to);
              if (!a || !b) return null;
              // Exit `from` on the side facing `to`, enter `to` on the side facing
              // `from`; arrow points at the dependency (`to`).
              const fromLeftOfTo = a.x + a.w / 2 < b.x + b.w / 2;
              const x1 = fromLeftOfTo ? a.x + a.w : a.x;
              const x2 = fromLeftOfTo ? b.x : b.x + b.w;
              const y1 = a.y + a.h / 2;
              const y2 = b.y + b.h / 2;
              const mx = (x1 + x2) / 2;
              const active = selectedId === e.from || selectedId === e.to;
              return (
                <path
                  key={i}
                  d={`M ${x1} ${y1} C ${mx} ${y1}, ${mx} ${y2}, ${x2} ${y2}`}
                  className={cn(active ? "stroke-brand" : "stroke-border-strong")}
                  strokeWidth={active ? 1.8 : 1.25}
                  opacity={active ? 0.72 : 0.34}
                  markerEnd={active ? "url(#weft-arrow-active)" : "url(#weft-arrow)"}
                />
              );
            })}
          </svg>

          {repoProfiles.map((p) => {
            const pt = layout.pos.get(p.repo_id);
            if (!pt) return null;
            const dependents = repoEdges.filter((e) => e.to === p.repo_id).length;
            const selected = selectedId === p.repo_id;
            const onSelect = () => {
              setSelectedId(p.repo_id);
              setProfileOpen(true);
            };
            const expanded = mode === "expanded" && p.components.length > 0;
            return expanded ? (
              <ExpandedNode
                key={p.repo_id}
                profile={p}
                pt={pt}
                selected={selected}
                onSelect={onSelect}
                onReprofile={() => void reprofileRepo(p.repo_id)}
              />
            ) : (
              <RepoNode
                key={p.repo_id}
                profile={p}
                pt={pt}
                selected={selected}
                dependents={dependents}
                showPkgs={mode === "expanded"}
                onSelect={onSelect}
                onReprofile={() => void reprofileRepo(p.repo_id)}
              />
            );
          })}
        </div>

        {/* Bottom toolbar: analyze / calibrate / view mode + kind filter / zoom. */}
        <div className="pointer-events-none absolute inset-x-4 bottom-4 flex items-end justify-between gap-3">
          <div className="pointer-events-none flex flex-col items-start gap-2">
            <div className="pointer-events-none flex items-center gap-2">
              <button
                data-graph-controls
                onClick={() => {
                  if (analyzing) return;
                  setAnalyzing(true);
                  void reanalyzeDeps().finally(() => setAnalyzing(false));
                }}
                disabled={analyzing}
                title={t("repomap.reanalyzeHint")}
                className="pointer-events-auto flex items-center gap-1.5 rounded-[var(--radius-md)] border border-border bg-raised px-2.5 py-1.5 text-[11.5px] text-ink-muted shadow-[0_4px_16px_-6px_rgba(0,0,0,0.4)] transition-colors hover:text-ink disabled:opacity-60"
              >
                <RefreshCw size={12} className={analyzing ? "animate-spin" : undefined} />
                {analyzing ? t("repomap.reanalyzing") : t("repomap.reanalyze")}
              </button>
              <button
                data-graph-controls
                onClick={() => void openCuratorChat()}
                title={t("repomap.calibrateHint")}
                className="pointer-events-auto flex items-center gap-1.5 rounded-[var(--radius-md)] border border-border bg-raised px-2.5 py-1.5 text-[11.5px] text-ink-muted shadow-[0_4px_16px_-6px_rgba(0,0,0,0.4)] transition-colors hover:text-ink"
              >
                <MessagesSquare size={12} />
                {t("repomap.calibrate")}
              </button>
              {anyComponents && (
                <div
                  data-graph-controls
                  className="pointer-events-auto flex items-center gap-0.5 rounded-[var(--radius-md)] border border-border bg-raised p-0.5 shadow-[0_4px_16px_-6px_rgba(0,0,0,0.4)]"
                >
                  <ModeBtn active={mode === "overview"} onClick={() => setMode("overview")} icon={Boxes}>
                    {t("repomap.viewOverview")}
                  </ModeBtn>
                  <ModeBtn active={mode === "expanded"} onClick={() => setMode("expanded")} icon={Layers}>
                    {t("repomap.viewExpanded")}
                  </ModeBtn>
                </div>
              )}
            </div>
            {repoEdges.length > 0 && (
              <div
                data-graph-controls
                className="pointer-events-auto flex items-center gap-1 rounded-[var(--radius-md)] border border-border bg-raised px-1.5 py-1 shadow-[0_4px_16px_-6px_rgba(0,0,0,0.4)]"
              >
                {KINDS.map((kind) => (
                  <button
                    key={kind}
                    onClick={() => toggleKind(kind)}
                    title={t("repomap.filterKind", { kind })}
                    className={cn(
                      "rounded px-1.5 py-0.5 text-[10px] font-medium uppercase transition-colors",
                      activeKinds.has(kind)
                        ? "bg-brand-ghost text-brand"
                        : "text-ink-faint line-through hover:text-ink-muted",
                    )}
                  >
                    {kind}
                  </button>
                ))}
              </div>
            )}
          </div>
          <div
            data-graph-controls
            className="pointer-events-auto flex items-center gap-0.5 rounded-[var(--radius-md)] border border-border bg-raised p-1 shadow-[0_4px_16px_-6px_rgba(0,0,0,0.4)]"
          >
            <ZoomBtn onClick={() => zoomButton(0.83)} label={t("repomap.zoomOut")}>
              <Minus size={14} />
            </ZoomBtn>
            <button
              onClick={fit}
              title={t("repomap.fit")}
              className="min-w-[44px] rounded px-1.5 py-1 text-center text-[11px] tabular-nums text-ink-muted transition-colors hover:bg-brand-ghost hover:text-ink"
            >
              {Math.round(zoom * 100)}%
            </button>
            <ZoomBtn onClick={() => zoomButton(1.2)} label={t("repomap.zoomIn")}>
              <Plus size={14} />
            </ZoomBtn>
            <ZoomBtn onClick={fit} label={t("repomap.fit")}>
              <Maximize2 size={13} />
            </ZoomBtn>
          </div>
        </div>
      </div>

      {profileOpen ? (
        <RepoProfilePane
          profile={repoProfiles.find((p) => p.repo_id === selectedId)}
          edges={repoEdges}
          profiles={repoProfiles}
          onSelect={(id) => {
            setSelectedId(id);
            setProfileOpen(true);
          }}
          onCollapse={() => setProfileOpen(false)}
        />
      ) : (
        <CollapsedProfileRail onOpen={() => setProfileOpen(true)} />
      )}
    </div>
  );
}

/** A standard repo node (overview, or expanded mode for a single-component repo). */
function RepoNode({
  profile: p,
  pt,
  selected,
  dependents,
  showPkgs,
  onSelect,
  onReprofile,
}: {
  profile: RepoProfile;
  pt: { x: number; y: number; w: number; h: number };
  selected: boolean;
  dependents: number;
  showPkgs: boolean;
  onSelect: () => void;
  onReprofile: () => void;
}) {
  const { t } = useTranslation();
  const Icon = TIER_ICON[bandOf(p.tier)] ?? CircleDashed;
  const core = dependents >= 2;
  return (
    <div
      data-repo-node
      onClick={onSelect}
      className={cn(
        "group absolute flex flex-col gap-2 overflow-hidden rounded-[var(--radius-md)] border bg-surface px-3 py-2.5 text-left transition-[transform,border-color,background-color] hover:-translate-y-px",
        selected
          ? "border-brand/60 bg-brand-ghost/60"
          : core
            ? "border-accent/50"
            : "border-border hover:border-border-strong",
        !p.analyzed && "border-dashed opacity-80",
      )}
      style={{ left: pt.x, top: pt.y, width: pt.w, height: pt.h }}
    >
      <div className="flex items-center gap-1.5">
        <span className="grid h-5 w-5 shrink-0 place-items-center rounded bg-raised">
          {p.analyzed ? (
            <Icon size={12} className={selected ? "text-brand" : "text-ink-muted"} />
          ) : (
            <Loader2 size={12} className="animate-spin text-ink-faint" />
          )}
        </span>
        <span title={p.repo_name} className="min-w-0 flex-1 truncate text-[13.5px] font-semibold text-ink">
          {p.repo_name}
        </span>
        {p.stale && (
          <span title={t("repomap.staleTitle")} className="h-1.5 w-1.5 shrink-0 rounded-full bg-waiting" />
        )}
        {showPkgs && p.components.length > 0 && (
          <span className="shrink-0 rounded-full bg-accent-ghost px-1.5 text-[10px] text-accent">
            {t("repomap.pkgCount", { count: p.components.length })}
          </span>
        )}
        <button
          onClick={(e) => {
            e.stopPropagation();
            onReprofile();
          }}
          aria-label={t("repomap.reprofile")}
          title={t("repomap.reprofile")}
          className="grid h-5 w-5 shrink-0 place-items-center rounded text-ink-faint opacity-0 transition-opacity hover:bg-brand-ghost hover:text-ink group-hover:opacity-100"
        >
          <RefreshCw size={11} />
        </button>
      </div>

      {p.analyzed ? (
        <>
          <NodeBadges tier={p.tier} stack={p.stack} core={core} dependents={dependents} />
          <NodeSummary profile={p} />
        </>
      ) : (
        <span className="text-[11.5px] italic text-ink-faint">{t("repomap.analyzing")}</span>
      )}
    </div>
  );
}

/** A monorepo container (expanded view): the repo's components grouped by tier. */
function ExpandedNode({
  profile: p,
  pt,
  selected,
  onSelect,
  onReprofile,
}: {
  profile: RepoProfile;
  pt: { x: number; y: number; w: number; h: number };
  selected: boolean;
  onSelect: () => void;
  onReprofile: () => void;
}) {
  const { t } = useTranslation();
  const Icon = TIER_ICON[bandOf(p.tier)] ?? CircleDashed;
  return (
    <div
      data-repo-node
      onClick={onSelect}
      className={cn(
        "group absolute flex flex-col overflow-hidden rounded-[var(--radius-md)] border bg-surface text-left",
        selected ? "border-brand/60 bg-brand-ghost/40" : "border-border hover:border-border-strong",
      )}
      style={{ left: pt.x, top: pt.y, width: pt.w, height: pt.h }}
    >
      <div className="flex items-center gap-1.5 border-b border-border px-3 py-2">
        <span className="grid h-5 w-5 shrink-0 place-items-center rounded bg-raised">
          <Icon size={12} className={selected ? "text-brand" : "text-ink-muted"} />
        </span>
        <span title={p.repo_name} className="min-w-0 flex-1 truncate text-[13.5px] font-semibold text-ink">
          {p.repo_name}
        </span>
        <span className="shrink-0 rounded-full bg-accent-ghost px-1.5 text-[10px] text-accent">
          {t("repomap.pkgCount", { count: p.components.length })}
        </span>
        <button
          onClick={(e) => {
            e.stopPropagation();
            onReprofile();
          }}
          aria-label={t("repomap.reprofile")}
          title={t("repomap.reprofile")}
          className="grid h-5 w-5 shrink-0 place-items-center rounded text-ink-faint opacity-0 transition-opacity hover:bg-brand-ghost hover:text-ink group-hover:opacity-100"
        >
          <RefreshCw size={11} />
        </button>
      </div>
      <div className="min-h-0 flex-1 overflow-hidden px-2 py-1.5">
        {groupComponents(p.components).map(([band, comps]) => {
          const TierIcon = TIER_ICON[band] ?? CircleDashed;
          return (
            <div key={band} className="mb-1">
              <div className="flex items-center gap-1 px-1 text-[9.5px] font-medium uppercase tracking-wide text-ink-faint">
                <TierIcon size={9} />
                {t(`repomap.tier_${band}`, band)}
              </div>
              {comps.map((c) => (
                <div
                  key={c.name}
                  title={c.summary || c.name}
                  className="mt-0.5 flex items-center gap-1.5 rounded border border-border/70 bg-bg px-1.5 py-1"
                >
                  <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-ink">{c.name}</span>
                  {c.deps.length > 0 && (
                    <span className="shrink-0 truncate text-[9.5px] text-ink-faint" title={c.deps.join(", ")}>
                      → {c.deps.join(", ")}
                    </span>
                  )}
                </div>
              ))}
            </div>
          );
        })}
      </div>
    </div>
  );
}

function ModeBtn({
  active,
  onClick,
  icon: Icon,
  children,
}: {
  active: boolean;
  onClick: () => void;
  icon: ComponentType<LucideProps>;
  children: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        "flex items-center gap-1 rounded px-2 py-1 text-[11px] transition-colors",
        active ? "bg-brand-ghost text-brand" : "text-ink-muted hover:text-ink",
      )}
    >
      <Icon size={12} />
      {children}
    </button>
  );
}

function RepoProfilePane({
  profile,
  profiles,
  edges,
  onSelect,
  onCollapse,
}: {
  profile?: RepoProfile;
  profiles: RepoProfile[];
  edges: RepoEdge[];
  onSelect: (id: number) => void;
  onCollapse: () => void;
}) {
  const { t } = useTranslation();
  const { reprofileRepo, deleteRepo } = useStore();
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  if (!profile) return <EmptyProfilePane />;

  const deps = edges
    .filter((e) => e.from === profile.repo_id)
    .map((e) => ({ edge: e, repo: profiles.find((p) => p.repo_id === e.to) }))
    .filter((x): x is { edge: RepoEdge; repo: RepoProfile } => !!x.repo);
  const usedBy = edges
    .filter((e) => e.to === profile.repo_id)
    .map((e) => ({ edge: e, repo: profiles.find((p) => p.repo_id === e.from) }))
    .filter((x): x is { edge: RepoEdge; repo: RepoProfile } => !!x.repo);
  const Icon = TIER_ICON[bandOf(profile.tier)] ?? CircleDashed;

  return (
    <aside className="flex w-[320px] shrink-0 flex-col overflow-hidden rounded-[var(--radius-lg)] border border-border bg-surface">
      <div className="border-b border-border px-4 py-3">
        <div className="flex min-h-10 items-center gap-2.5">
          <span className="grid h-8 w-8 shrink-0 place-items-center rounded-[var(--radius-md)] bg-brand-ghost">
            <Icon size={16} className="text-brand" />
          </span>
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <h2 className="truncate font-mono text-[16px] font-semibold text-ink">{profile.repo_name}</h2>
              <TierBadge profile={profile} />
            </div>
            {profile.stale && (
              <span className="mt-1 inline-flex text-[11px] text-waiting">{t("repomap.stale")}</span>
            )}
          </div>
          <button
            onClick={() => void reprofileRepo(profile.repo_id)}
            title={t("repomap.reprofile")}
            className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
          >
            <RefreshCw size={14} />
          </button>
          <button
            onClick={() => setConfirmDelete(true)}
            title={t("repomap.deleteRepo")}
            className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-danger/10 hover:text-danger"
          >
            <Trash2 size={14} />
          </button>
          <button
            onClick={onCollapse}
            aria-label={t("repomap.collapseProfile")}
            title={t("repomap.collapseProfile")}
            className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
          >
            <PanelRightClose size={14} />
          </button>
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-auto px-4 py-4">
        {!profile.analyzed && (
          <div className="mb-4 flex items-center gap-2 rounded-[var(--radius-md)] border border-dashed border-border bg-bg px-3 py-2 text-[12px] text-ink-faint">
            <Loader2 size={13} className="animate-spin" />
            {t("repomap.analyzing")}
          </div>
        )}

        <ProfileSection title={t("repomap.oneLine")}>
          <NodeSummary profile={profile} />
        </ProfileSection>

        <div className="grid grid-cols-2 gap-3">
          <ProfileSection title={t("repomap.tier")}>
            <TierPicker profile={profile} />
          </ProfileSection>
          <ProfileSection title={t("repomap.source")}>
            <span className="text-[13px] text-ink-muted">
              {profile.source
                ? t(`repomap.source_${profile.source.startsWith("user") ? "user" : profile.source}`, profile.source)
                : t("repomap.none")}
            </span>
          </ProfileSection>
        </div>

        <ProfileSection title={t("repomap.stack")}>
          <ChipList values={profile.stack} empty={t("repomap.none")} mono />
        </ProfileSection>

        {profile.components.length > 0 && (
          <ProfileSection title={t("repomap.components")}>
            <ComponentList components={profile.components} />
          </ProfileSection>
        )}

        <ProfileSection title={t("repomap.dependsOn")}>
          <RepoLinks items={deps} empty={t("repomap.noDeps")} onSelect={onSelect} />
        </ProfileSection>

        <ProfileSection title={t("repomap.usedBy")}>
          <RepoLinks items={usedBy} empty={t("repomap.noUsedBy")} onSelect={onSelect} reverse />
        </ProfileSection>

        {profile.profiled_commit && (
          <div className="mt-4 flex items-center gap-1.5 text-[11px] text-ink-faint">
            <GitBranch size={12} />
            <span>{t("repomap.profiledAt")}</span>
            <span className="font-mono">{profile.profiled_commit.slice(0, 8)}</span>
          </div>
        )}
      </div>

      <Dialog open={confirmDelete} onOpenChange={(o) => !deleting && setConfirmDelete(o)}>
        <DialogContent title={t("repomap.deleteRepoTitle", { name: profile.repo_name })}>
          <p className="text-[13px] leading-relaxed text-ink-muted">{t("repomap.deleteRepoBody")}</p>
          <div className="mt-4 flex justify-end gap-2">
            <Button variant="ghost" disabled={deleting} onClick={() => setConfirmDelete(false)}>
              {t("common.cancel")}
            </Button>
            <Button
              variant="danger"
              disabled={deleting}
              onClick={() => {
                setDeleting(true);
                void deleteRepo(profile.repo_id)
                  .then(() => setConfirmDelete(false))
                  .finally(() => setDeleting(false));
              }}
            >
              {deleting ? t("repomap.deleting") : t("repomap.deleteRepoConfirm")}
            </Button>
          </div>
        </DialogContent>
      </Dialog>
    </aside>
  );
}

function TierBadge({ profile }: { profile: RepoProfile }) {
  const { t } = useTranslation();
  const label = profile.tier
    ? t(`repomap.tier_${profile.tier}`, profile.tier)
    : t("repomap.tier_other");
  return (
    <span className="shrink-0 rounded-full border border-border bg-bg px-2 py-0.5 text-[11px] text-ink-muted">
      {label}
    </span>
  );
}

/** Calibrate a repo's tier (a user pick is pinned, outranking the agent). */
function TierPicker({ profile }: { profile: RepoProfile }) {
  const { editRepoTier } = useStore();
  const { t } = useTranslation();
  const canonical = (TIER_ORDER as readonly string[]).includes(profile.tier);
  return (
    <select
      value={canonical ? profile.tier : ""}
      onChange={(e) => void editRepoTier(profile.repo_id, e.currentTarget.value)}
      className="w-full rounded border border-border bg-bg px-1.5 py-1 text-[12.5px] text-ink outline-none focus:border-brand/60"
    >
      {/* The empty option is a non-selectable placeholder for the unclassified /
          analyzing state — tier is agent-owned, so the only user picks are the
          three real tiers (no user-pinnable "Other"). */}
      {!canonical && (
        <option value="" disabled>
          {t("repomap.tier_other")}
        </option>
      )}
      {TIER_ORDER.map((tier) => (
        <option key={tier} value={tier}>
          {t(`repomap.tier_${tier}`)}
        </option>
      ))}
    </select>
  );
}

function ComponentList({ components }: { components: RepoComponent[] }) {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col gap-1.5">
      {components.map((c) => {
        const Icon = TIER_ICON[bandOf(c.tier)] ?? CircleDashed;
        return (
          <div key={c.name} className="rounded-[var(--radius-md)] border border-border bg-bg px-2.5 py-2">
            <div className="flex items-center gap-1.5">
              <Icon size={11} className="shrink-0 text-ink-faint" />
              <span className="min-w-0 flex-1 truncate font-mono text-[12px] text-ink">{c.name}</span>
              <span className="shrink-0 text-[10px] uppercase text-ink-faint">
                {c.tier ? t(`repomap.tier_${c.tier}`, c.tier) : t("repomap.tier_other")}
              </span>
            </div>
            {c.summary && <p className="mt-0.5 text-[11px] leading-snug text-ink-muted">{c.summary}</p>}
            {c.deps.length > 0 && (
              <p className="mt-0.5 text-[10.5px] text-ink-faint">{t("repomap.via", { via: c.deps.join(", ") })}</p>
            )}
          </div>
        );
      })}
    </div>
  );
}

function CollapsedProfileRail({ onOpen }: { onOpen: () => void }) {
  const { t } = useTranslation();
  return (
    <button
      type="button"
      onClick={onOpen}
      aria-label={t("repomap.expandProfile")}
      title={t("repomap.expandProfile")}
      className="flex w-10 shrink-0 items-start justify-center rounded-[var(--radius-lg)] border border-border bg-surface pt-3 text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
    >
      <PanelRightOpen size={15} />
    </button>
  );
}

function EmptyProfilePane() {
  const { t } = useTranslation();
  return (
    <aside className="flex w-[320px] shrink-0 flex-col items-center justify-center rounded-[var(--radius-lg)] border border-border bg-surface px-5 text-center">
      <CircleDashed size={22} className="text-ink-faint" />
      <p className="mt-3 text-[13px] font-medium text-ink">{t("repomap.selectRepo")}</p>
      <p className="mt-1 text-[12px] leading-relaxed text-ink-faint">{t("repomap.selectRepoBody")}</p>
    </aside>
  );
}

function NodeBadges({
  tier,
  stack,
  core,
  dependents,
}: {
  tier: string;
  stack: string[];
  core: boolean;
  dependents: number;
}) {
  const { t } = useTranslation();
  const visibleStack = stack.slice(0, 2);
  const hiddenStack = stack.slice(2);
  const tierLabel = tier ? t(`repomap.tier_${tier}`, tier) : t("repomap.tier_other");

  return (
    <div className="flex min-h-[20px] min-w-0 items-center gap-1 overflow-hidden">
      <MiniPill title={tierLabel} rounded>
        {tierLabel}
      </MiniPill>
      {visibleStack.map((s) => (
        <MiniPill key={s} title={s} mono>
          {s}
        </MiniPill>
      ))}
      {hiddenStack.length > 0 && (
        <MiniPill title={hiddenStack.join(", ")} mono>
          +{hiddenStack.length}
        </MiniPill>
      )}
      {core && (
        <span
          title={t("repomap.rippleTitle", { count: dependents })}
          className="ml-auto inline-flex h-5 min-w-5 shrink-0 items-center justify-center rounded-full bg-accent-ghost px-1.5 text-[10px] font-medium tabular-nums text-accent"
        >
          {dependents}
        </span>
      )}
    </div>
  );
}

function MiniPill({
  title,
  mono,
  rounded,
  children,
}: {
  title: string;
  mono?: boolean;
  rounded?: boolean;
  children: React.ReactNode;
}) {
  return (
    <span
      title={title}
      className={cn(
        "min-w-0 max-w-[76px] shrink-0 truncate bg-bg px-1.5 py-px text-[10.5px] leading-5 text-ink-faint",
        mono && "font-mono",
        rounded ? "rounded-full" : "rounded-[var(--radius-sm)]",
      )}
    >
      {children}
    </span>
  );
}

function ProfileSection({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="mb-4">
      <h3 className="mb-1.5 text-[11px] font-medium uppercase text-ink-faint">{title}</h3>
      {children}
    </section>
  );
}

function ChipList({ values, empty, mono }: { values: string[]; empty: string; mono?: boolean }) {
  if (values.length === 0) return <span className="text-[13px] text-ink-faint">{empty}</span>;
  return (
    <div className="flex flex-wrap gap-1.5">
      {values.map((value) => (
        <span
          key={value}
          className={cn(
            "rounded-[var(--radius-sm)] border border-border bg-bg px-2 py-1 text-[11px] text-ink-muted",
            mono && "font-mono",
          )}
        >
          {value}
        </span>
      ))}
    </div>
  );
}

function RepoLinks({
  items,
  empty,
  onSelect,
  reverse,
}: {
  items: { repo: RepoProfile; edge: { via: string; kind?: string } }[];
  empty: string;
  onSelect: (id: number) => void;
  reverse?: boolean;
}) {
  const { t } = useTranslation();
  if (items.length === 0) return <span className="text-[13px] text-ink-faint">{empty}</span>;
  return (
    <div className="flex flex-col gap-1.5">
      {items.map(({ repo, edge }) => (
        <button
          key={repo.repo_id}
          onClick={() => onSelect(repo.repo_id)}
          className="flex items-center gap-2 rounded-[var(--radius-md)] border border-border bg-bg px-2.5 py-2 text-left transition-colors hover:border-border-strong hover:bg-raised"
        >
          <span className="min-w-0 flex-1 truncate font-mono text-[12px] text-ink">
            {reverse ? `${repo.repo_name}` : repo.repo_name}
          </span>
          {edge.kind && (
            <span className="shrink-0 rounded bg-brand-ghost px-1.5 py-0.5 text-[10px] font-medium uppercase text-brand">
              {edge.kind}
            </span>
          )}
          {edge.via && (
            <span className="max-w-[120px] truncate text-[11px] text-ink-faint">
              {t("repomap.via", { via: edge.via })}
            </span>
          )}
        </button>
      ))}
    </div>
  );
}

/** The node's one-line summary, click-to-edit (correcting it teaches the map). */
function NodeSummary({ profile }: { profile: RepoProfile }) {
  const { editRepoSummary } = useStore();
  const { t } = useTranslation();
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(profile.summary);

  async function save() {
    setEditing(false);
    const next = text.trim();
    if (next === profile.summary) return;
    await editRepoSummary(profile.repo_id, next);
  }

  if (editing) {
    return (
      <input
        autoFocus
        value={text}
        onChange={(e) => setText(e.currentTarget.value)}
        onBlur={() => void save()}
        onKeyDown={(e) => {
          if (e.key === "Enter") void save();
          if (e.key === "Escape") {
            setText(profile.summary);
            setEditing(false);
          }
        }}
        placeholder={t("repomap.summaryPlaceholder")}
        className="w-full rounded border border-border bg-bg px-1.5 py-1 text-[11.5px] text-ink outline-none focus:border-brand/60"
      />
    );
  }

  return (
    <button
      onClick={() => {
        setText(profile.summary);
        setEditing(true);
      }}
      title={t("repomap.editHint")}
      className="group/sum flex min-w-0 items-start gap-1 text-left"
    >
      <span
        className={cn(
          "min-w-0 text-[11.5px] leading-snug",
          profile.summary ? "text-ink-muted" : "text-ink-faint italic",
        )}
        style={{
          display: "-webkit-box",
          WebkitLineClamp: 2,
          WebkitBoxOrient: "vertical",
          overflow: "hidden",
        }}
      >
        {profile.summary || t("repomap.addSummary")}
      </span>
      {ownsSummary(profile.source) && (
        <span className="mt-px shrink-0 rounded bg-brand-ghost px-1 py-px text-[9px] font-medium text-brand">
          {t("repomap.yours")}
        </span>
      )}
      <Pencil
        size={10}
        className="mt-0.5 shrink-0 text-ink-faint opacity-0 transition-opacity group-hover/sum:opacity-100"
      />
    </button>
  );
}

function ZoomBtn({
  onClick,
  label,
  children,
}: {
  onClick: () => void;
  label: string;
  children: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      aria-label={label}
      title={label}
      className="grid h-7 w-7 place-items-center rounded text-ink-muted transition-colors hover:bg-brand-ghost hover:text-ink"
    >
      {children}
    </button>
  );
}
