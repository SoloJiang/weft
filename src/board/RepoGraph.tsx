import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  AppWindow,
  Boxes,
  CircleDashed,
  GitBranch,
  Layers,
  Maximize2,
  Minus,
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
import { Select } from "../components/ui/Select";
import { cn } from "../lib/cn";

/** The two architectural tiers laid out left→right, plus the catch-all "other"
 *  band that holds unclassified / still-analyzing repos. */
const TIER_ORDER = ["frontend", "backend"] as const;
const BANDS = ["frontend", "backend", "other"] as const;
type Band = (typeof BANDS)[number];

const TIER_ICON: Record<string, ComponentType<LucideProps>> = {
  frontend: AppWindow,
  backend: Server,
  other: CircleDashed,
};


const bandOf = (tier: string): Band =>
  (TIER_ORDER as readonly string[]).includes(tier) ? (tier as Band) : "other";

// ─────────────────── repo analysis display status ───────────────────
// All analysis runs through the workspace curator (仓库分析助手) now — the map is a
// PROFILE surface, not a per-repo analysis lifecycle. A node is either classified
// ("analyzed") or not yet ("pending", a passive "未分析" hint). No per-node
// running/failed status or retry: progress and failures live in the curator chat.

type AnalysisView = "analyzed" | "pending";

function analysisView(p: RepoProfile): AnalysisView {
  return p.analyzed ? "analyzed" : "pending";
}

/** Status-driven border accent for a card. Selection/importance are separate
 *  axes layered on top via `cn` (see `cardFrame`). Keyed by the discriminant, so
 *  a new view forces a new entry — exhaustive by construction. */
const CARD_STATUS_FRAME: Record<AnalysisView, string> = {
  pending: "border-dashed opacity-80",
  analyzed: "",
};

/** Selection axis for a card border — orthogonal to analysis status. Importance is
 *  no longer accented: with dependency lines and the dependents badge gone, a lone
 *  colored "core" border was an unexplained signal, so all unselected cards share
 *  the neutral border. */
function cardFrame(selected: boolean): string {
  if (selected) return "border-brand/60 bg-brand-ghost/60";
  return "border-border hover:border-border-strong";
}

/** Border for an expanded (monorepo) card: selection wins. */
function expandedFrame(selected: boolean): string {
  if (selected) return "border-brand/60 bg-brand-ghost/40";
  return "border-border hover:border-border-strong";
}

/** Whether the user has pinned the summary (mirrors the backend `source` flags:
 *  "user" = both fields owned, "user_summary" = just the summary). */
const ownsSummary = (source: string): boolean => source === "user" || source === "user_summary";

type ViewMode = "overview" | "expanded";

const NODE_W = 248;
const NODE_H = 108;
const ROW_GAP = 18;
const PAD = 18;
// Expanded-container metrics (a repo's monorepo components grouped by tier).
const EXP_HEADER_H = 60;
const EXP_TIER_SUB_H = 20;
const EXP_COMP_H = 40;
// Stacked-band layout metrics. Each architectural layer is a horizontal band;
// cards flow left→right and wrap into rows after CARDS_PER_ROW so a fat layer
// (e.g. all the IDL/common libs) becomes a couple of rows instead of overflowing.
const CARDS_PER_ROW = 7;
const CARD_GAP_X = 18; // horizontal gap between cards within a row
const BAND_HEADER_H = 30; // the layer label strip above a band's cards
const BAND_PAD = 14; // inner padding inside a band, around the cards
const BAND_GAP_V = 22; // vertical gap between stacked bands
const MIN_Z = 0.35;
const MAX_Z = 2.5;
const clampZ = (z: number) => Math.min(MAX_Z, Math.max(MIN_Z, z));

/** A repo's architectural layer for the map. The cross-repo curator pass assigns an
 *  agent-named `layer` + `layer_rank` (higher rank = closer to the user); when that
 *  hasn't run yet we fall back to a generic band derived from the agent's existing
 *  category/tier signal (NOT repo names), so the map still stacks sensibly and the
 *  moment the curator assigns real layers they take over. */
function layerBandOf(p: RepoProfile, t: (k: string) => string): { label: string; rank: number } {
  if (p.layer.trim()) return { label: p.layer.trim(), rank: p.layer_rank };
  const c = (p.category ?? "").toLowerCase();
  const has = (s: string) => c.includes(s);
  if (has("task") || has("worker") || has("support")) return { label: t("repomap.layer_support"), rank: 2 };
  if (has("gateway") || has("edge") || has("bff")) return { label: t("repomap.layer_gateway"), rank: 5 };
  if (has("biz") || has("aggreg")) return { label: t("repomap.layer_biz"), rank: 4 };
  if (has("core")) return { label: t("repomap.layer_core"), rank: 3 };
  if (has("common") || has("idl") || has("cache") || has("proto") || has("lib") || has("shared"))
    return { label: t("repomap.layer_foundation"), rank: 1 };
  if (has("sdk") || has("app") || has("web") || has("client") || has("desktop"))
    return { label: t("repomap.layer_client"), rank: 6 };
  if (p.tier === "frontend") return { label: t("repomap.layer_client"), rank: 6 };
  if (p.tier === "backend") return { label: t("repomap.layer_biz"), rank: 4 };
  return { label: t("repomap.layer_other"), rank: 0 };
}

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
 * The repo map as a pan/zoom canvas — the whole Repos surface. Repos are stacked
 * into architectural-layer bands (agent-assigned `layer`/`layer_rank` from the
 * cross-repo curator pass; a generic category/tier fallback until it runs), top →
 * bottom by rank, so the stacking itself states "上层依赖下层". No dependency lines
 * are drawn at all — a repo's deps / dependents live in its detail panel. Switch to
 * the expanded view to break monorepos into their components. Drag to pan, scroll to zoom.
 */
export function RepoGraph() {
  const { repoProfiles, repoEdges, reanalyzeDeps, analyzing, selectedRepoId, openRepoDetail } = useStore();
  const { t } = useTranslation();
  const [mode, setMode] = useState<ViewMode>("overview");

  const layout = useMemo(() => {
    // Topological depth (memoised DFS, cycle-guarded) — the within-layer ordering
    // hint so a layer's most-foundational repos sit leftmost in their band.
    const depsOf = new Map<number, number[]>();
    for (const p of repoProfiles) depsOf.set(p.repo_id, []);
    for (const e of repoEdges) {
      const d = depsOf.get(e.from);
      if (d) d.push(e.to);
    }
    const depthMemo = new Map<number, number>();
    const visiting = new Set<number>();
    function computeDepth(id: number): number {
      if (depthMemo.has(id)) return depthMemo.get(id)!;
      if (visiting.has(id)) return 0;
      visiting.add(id);
      const myDeps = depsOf.get(id) ?? [];
      const d = myDeps.length === 0 ? 0 : 1 + Math.max(0, ...myDeps.map(computeDepth));
      visiting.delete(id);
      depthMemo.set(id, d);
      return d;
    }
    for (const p of repoProfiles) computeDepth(p.repo_id);
    const inDegree = (id: number) => repoEdges.filter((e) => e.to === id).length;

    // Bucket repos by their agent-assigned layer label. Each band's vertical rank is
    // the max rank among its members — tolerant of a stray inconsistent rank from the
    // agent without splitting a layer. Cards flow left→right and wrap into rows.
    const byLabel = new Map<string, RepoProfile[]>();
    const rankOf = new Map<number, number>();
    for (const p of repoProfiles) {
      const { label, rank } = layerBandOf(p, t);
      rankOf.set(p.repo_id, rank);
      const arr = byLabel.get(label) ?? [];
      arr.push(p);
      byLabel.set(label, arr);
    }
    const bandRank = new Map<string, number>();
    for (const [label, arr] of byLabel) {
      bandRank.set(label, Math.max(...arr.map((p) => rankOf.get(p.repo_id) ?? 0)));
    }
    for (const arr of byLabel.values()) {
      arr.sort(
        (a, b) =>
          (depthMemo.get(a.repo_id) ?? 0) - (depthMemo.get(b.repo_id) ?? 0) ||
          inDegree(b.repo_id) - inDegree(a.repo_id) ||
          a.repo_name.localeCompare(b.repo_name),
      );
    }
    // Stack bands top → bottom by rank DESC (highest rank = closest to the user).
    const orderedLabels = Array.from(byLabel.keys()).sort(
      (a, b) => (bandRank.get(b) ?? 0) - (bandRank.get(a) ?? 0) || a.localeCompare(b),
    );

    const contentW = CARDS_PER_ROW * NODE_W + (CARDS_PER_ROW - 1) * CARD_GAP_X;
    const width = PAD + BAND_PAD + contentW + BAND_PAD + PAD;
    const pos = new Map<number, { x: number; y: number; w: number; h: number }>();
    const bands: { label: string; count: number; top: number; height: number }[] = [];
    let y = PAD;

    for (const label of orderedLabels) {
      const arr = byLabel.get(label);
      if (!arr || arr.length === 0) continue;
      const top = y;
      let cardY = top + BAND_HEADER_H;
      for (let i = 0; i < arr.length; i += CARDS_PER_ROW) {
        const row = arr.slice(i, i + CARDS_PER_ROW);
        const rowH = Math.max(...row.map((p) => nodeHeight(p, mode)));
        for (let c = 0; c < row.length; c++) {
          const x = PAD + BAND_PAD + c * (NODE_W + CARD_GAP_X);
          pos.set(row[c].repo_id, { x, y: cardY, w: NODE_W, h: nodeHeight(row[c], mode) });
        }
        cardY += rowH + ROW_GAP;
      }
      const height = cardY - ROW_GAP + BAND_PAD - top;
      bands.push({ label, count: arr.length, top, height });
      y = top + height + BAND_GAP_V;
    }

    const height = Math.max(NODE_H + PAD * 2, y - BAND_GAP_V + PAD);
    return { pos, bands, width, contentW, height };
  }, [repoProfiles, repoEdges, mode, t]);

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

  // Re-fit when the container itself resizes — e.g. when the side panel (detail /
  // curator) opens, closes, or is drag-resized and shrinks this canvas. fit() reads
  // clientWidth/Height, but only ran on first paint / graph-shape change, so without
  // this the graph stayed scaled for the old width and nodes were clipped until a
  // manual Fit. rAF-coalesced so a drag-resize doesn't thrash.
  useEffect(() => {
    const el = containerRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    let raf = 0;
    const ro = new ResizeObserver(() => {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(() => fit());
    });
    ro.observe(el);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
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
    <div className="h-full min-h-0 w-full bg-bg p-4">
      <div
        ref={containerRef}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={endDrag}
        onPointerLeave={endDrag}
        className="relative h-full w-full cursor-grab select-none overflow-hidden rounded-[var(--radius-lg)] border border-border bg-surface/35 [touch-action:none] active:cursor-grabbing"
      >
        <div
          className="absolute left-0 top-0 origin-top-left"
          style={{
            width: layout.width,
            height: layout.height,
            transform: `translate(${pan.x}px, ${pan.y}px) scale(${zoom})`,
          }}
        >
          {/* Architectural-layer bands (behind the cards): one labeled strip per
              layer, stacked top → bottom by rank. The stacking IS the dependency
              statement ("上层依赖下层"), so no resting edges are drawn. */}
          {layout.bands.map((band) => (
            <div
              key={band.label}
              className="absolute rounded-[var(--radius-lg)] border border-border/40 bg-surface/20"
              style={{ left: PAD / 2, top: band.top, width: layout.width - PAD, height: band.height }}
            >
              <div className="flex items-center gap-2 px-3 pt-1.5">
                <span className="truncate text-[12.5px] font-semibold text-ink-muted" title={band.label}>
                  {band.label}
                </span>
                <span className="shrink-0 text-[11px] tabular-nums text-ink-faint">{band.count}</span>
              </div>
            </div>
          ))}

          {repoProfiles.map((p) => {
            const pt = layout.pos.get(p.repo_id);
            if (!pt) return null;
            const selected = selectedRepoId === p.repo_id;
            const onSelect = () => openRepoDetail(p.repo_id);
            const expanded = mode === "expanded" && p.components.length > 0;
            return expanded ? (
              <ExpandedNode
                key={p.repo_id}
                profile={p}
                pt={pt}
                selected={selected}
                onSelect={onSelect}
              />
            ) : (
              <RepoNode
                key={p.repo_id}
                profile={p}
                pt={pt}
                selected={selected}
                onSelect={onSelect}
              />
            );
          })}
        </div>

        {/* Bottom toolbar: analyze / calibrate / view mode + kind filter / zoom.
            flex-wrap + shrink-0 keep each group at its content width and let them
            stack on a narrow panel, instead of flexbox squeezing the buttons until
            their CJK labels wrap one character per line. */}
        <div className="pointer-events-none absolute inset-x-4 bottom-4 flex flex-wrap items-end justify-between gap-2">
          <div className="pointer-events-none flex shrink-0 flex-wrap items-center gap-2">
            <button
              data-graph-controls
              onClick={() => void reanalyzeDeps()}
              disabled={analyzing}
              title={t("repomap.reanalyzeHint")}
              className="pointer-events-auto flex shrink-0 items-center gap-1.5 whitespace-nowrap rounded-[var(--radius-md)] border border-border bg-raised px-2.5 py-1.5 text-[11.5px] text-ink-muted shadow-[0_4px_16px_-6px_rgba(0,0,0,0.4)] transition-colors hover:text-ink disabled:opacity-60"
            >
              <RefreshCw size={12} className={analyzing ? "animate-spin" : undefined} />
              {t("repomap.reanalyze")}
            </button>
            {anyComponents && (
              <div
                data-graph-controls
                className="pointer-events-auto flex shrink-0 items-center gap-0.5 rounded-[var(--radius-md)] border border-border bg-raised p-0.5 shadow-[0_4px_16px_-6px_rgba(0,0,0,0.4)]"
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
          <div
            data-graph-controls
            className="pointer-events-auto ml-auto flex shrink-0 items-center gap-0.5 rounded-[var(--radius-md)] border border-border bg-raised p-1 shadow-[0_4px_16px_-6px_rgba(0,0,0,0.4)]"
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
    </div>
  );
}

/** A standard repo node (overview, or expanded mode for a single-component repo). */
/** The leading glyph: the repo's tier icon (CircleDashed for an unclassified repo,
 *  since its tier band is "other"). */
function NodeStatusGlyph({
  selected,
  Icon,
}: {
  selected: boolean;
  Icon: ComponentType<LucideProps>;
}) {
  return <Icon size={12} className={selected ? "text-brand" : "text-ink-muted"} />;
}

/** A collapsed card's body: classified badges + summary, or a passive "未分析" hint
 *  for a not-yet-analyzed repo (analysis happens in the curator chat). */
function NodeStatusBody({ view, p }: { view: AnalysisView; p: RepoProfile }) {
  const { t } = useTranslation();
  if (view === "analyzed") return <NodeSummary profile={p} />;
  return <span className="text-[11.5px] italic text-ink-faint">{t("repomap.notAnalyzed")}</span>;
}

function RepoNode({
  profile: p,
  pt,
  selected,
  onSelect,
}: {
  profile: RepoProfile;
  pt: { x: number; y: number; w: number; h: number };
  selected: boolean;
  onSelect: () => void;
}) {
  const Icon = TIER_ICON[bandOf(p.tier)] ?? CircleDashed;
  const view = analysisView(p);

  return (
    <div
      data-repo-node
      onClick={onSelect}
      className={cn(
        "group absolute flex flex-col gap-2 overflow-hidden rounded-[var(--radius-md)] border bg-surface px-3 py-2.5 text-left transition-[transform,border-color,background-color] hover:-translate-y-px",
        cardFrame(selected),
        CARD_STATUS_FRAME[view],
      )}
      style={{ left: pt.x, top: pt.y, width: pt.w, height: pt.h }}
    >
      <div className="flex items-center gap-1.5">
        <span className="grid h-5 w-5 shrink-0 place-items-center rounded bg-raised">
          <NodeStatusGlyph selected={selected} Icon={Icon} />
        </span>
        <span title={p.repo_name} className="min-w-0 flex-1 truncate text-[13.5px] font-semibold text-ink">
          {p.repo_name}
        </span>
      </div>

      <NodeStatusBody view={view} p={p} />
    </div>
  );
}

/** A monorepo container (expanded view): the repo's components grouped by tier. */
function ExpandedNode({
  profile: p,
  pt,
  selected,
  onSelect,
}: {
  profile: RepoProfile;
  pt: { x: number; y: number; w: number; h: number };
  selected: boolean;
  onSelect: () => void;
}) {
  const { t } = useTranslation();
  const Icon = TIER_ICON[bandOf(p.tier)] ?? CircleDashed;
  return (
    <div
      data-repo-node
      onClick={onSelect}
      className={cn(
        "group absolute flex flex-col overflow-hidden rounded-[var(--radius-md)] border bg-surface text-left",
        expandedFrame(selected),
      )}
      style={{ left: pt.x, top: pt.y, width: pt.w, height: pt.h }}
    >
      <div className="flex items-center gap-1.5 border-b border-border px-3 py-2">
        <span className="grid h-5 w-5 shrink-0 place-items-center rounded bg-raised">
          <NodeStatusGlyph selected={selected} Icon={Icon} />
        </span>
        <span title={p.repo_name} className="min-w-0 flex-1 truncate text-[13.5px] font-semibold text-ink">
          {p.repo_name}
        </span>
        {p.category && (
          <span className="shrink-0 rounded bg-bg px-1.5 py-px text-[9.5px] uppercase text-ink-faint" title={p.category}>
            {p.category}
          </span>
        )}
        <span className="shrink-0 rounded-full bg-accent-ghost px-1.5 text-[10px] text-accent">
          {t("repomap.pkgCount", { count: p.components.length })}
        </span>
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
        "flex shrink-0 items-center gap-1 whitespace-nowrap rounded px-2 py-1 text-[11px] transition-colors",
        active ? "bg-brand-ghost text-brand" : "text-ink-muted hover:text-ink",
      )}
    >
      <Icon size={12} />
      {children}
    </button>
  );
}

/** A passive "not analyzed yet" banner above the (still-editable) fields. Analysis
 *  runs through the curator chat now, so there's no per-repo run button here. */
function PendingNotice() {
  const { t } = useTranslation();
  return (
    <div className="mb-4 rounded-[var(--radius-md)] border border-dashed border-border bg-bg px-3 py-2 text-[12px] text-ink-faint">
      {t("repomap.notAnalyzed")}
    </div>
  );
}

/** The classified-repo fields, shown once analysis is done and not re-running. */
function AnalyzedProfileFields({
  profile,
  deps,
  usedBy,
  onSelect,
  notice,
}: {
  profile: RepoProfile;
  deps: { edge: RepoEdge; repo: RepoProfile }[];
  usedBy: { edge: RepoEdge; repo: RepoProfile }[];
  onSelect: (id: number) => void;
  /** Optional status banner shown above the fields (failed / not-analyzed). The
   *  fields themselves stay editable so a placeholder/failed repo can be manually
   *  tiered + summarized — the backend supports editing placeholder profiles. */
  notice?: React.ReactNode;
}) {
  const { t } = useTranslation();
  return (
    <div className="h-full overflow-x-hidden overflow-y-auto px-4 py-4">
      {notice}
      <ProfileSection title={t("repomap.oneLine")}>
        <EditableSummary profile={profile} />
      </ProfileSection>

      <ProfileSection title={t("repomap.tier")}>
        <TierPicker profile={profile} />
      </ProfileSection>

      <ProfileSection title={t("repomap.stack")}>
        <ChipList values={profile.stack} empty={t("repomap.none")} mono />
      </ProfileSection>

      {profile.domains.length > 0 && (
        <ProfileSection title={t("repomap.domains")}>
          <ChipList values={profile.domains} empty={t("repomap.none")} />
        </ProfileSection>
      )}

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
  );
}

/** Detail body for the drawer: same content as the old RepoProfilePane, but
 *  store-driven (by repoId), without the fixed-width aside shell or the collapse
 *  button (the drawer owns close). */
export function RepoDetailContent({ repoId }: { repoId: number | null }) {
  const { t } = useTranslation();
  const { repoProfiles, repoEdges, deleteRepo, openRepoDetail } = useStore();
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const profile = repoId != null ? repoProfiles.find((p) => p.repo_id === repoId) : undefined;
  if (!profile) return <EmptyProfileBody />;

  const deps = repoEdges
    .filter((e) => e.from === profile.repo_id)
    .map((e) => ({ edge: e, repo: repoProfiles.find((p) => p.repo_id === e.to) }))
    .filter((x): x is { edge: RepoEdge; repo: RepoProfile } => !!x.repo);
  const usedBy = repoEdges
    .filter((e) => e.to === profile.repo_id)
    .map((e) => ({ edge: e, repo: repoProfiles.find((p) => p.repo_id === e.from) }))
    .filter((x): x is { edge: RepoEdge; repo: RepoProfile } => !!x.repo);
  const Icon = TIER_ICON[bandOf(profile.tier)] ?? CircleDashed;
  // Profile-only surface: classified repo shows its fields; an unanalyzed one shows
  // a passive "未分析" hint above the (still-editable) fields. Analysis itself runs
  // through the curator chat — no per-repo run/retry here.
  const paneBody = (
    <AnalyzedProfileFields
      profile={profile}
      deps={deps}
      usedBy={usedBy}
      onSelect={openRepoDetail}
      notice={profile.analyzed ? undefined : <PendingNotice />}
    />
  );

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="border-b border-border px-4 py-3">
        <div className="flex min-h-10 items-center gap-2.5">
          <span className="grid h-8 w-8 shrink-0 place-items-center rounded-[var(--radius-md)] bg-brand-ghost">
            <Icon size={16} className="text-brand" />
          </span>
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <h2 className="truncate font-mono text-[16px] font-semibold text-ink">{profile.repo_name}</h2>
              <TierBadge profile={profile} />
              {profile.category && (
                <span className="shrink-0 rounded-full border border-border bg-bg px-2 py-0.5 text-[11px] text-ink-muted">
                  {profile.category}
                </span>
              )}
            </div>
          </div>
          <button
            onClick={() => setConfirmDelete(true)}
            title={t("repomap.deleteRepo")}
            className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-danger/10 hover:text-danger"
          >
            <Trash2 size={14} />
          </button>
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-hidden">{paneBody}</div>

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
    </div>
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
    // The shared Radix/shadcn Select gives a properly-sized trigger (h-8). Tier is
    // agent-owned, so the only user picks are the three real tiers; an empty value
    // (an unclassified placeholder) falls back to the "未分类" placeholder text.
    <div className="max-w-[200px]">
      <Select
        value={canonical ? profile.tier : ""}
        onValueChange={(v) => void editRepoTier(profile.repo_id, v)}
        options={TIER_ORDER.map((tier) => ({ value: tier, label: t(`repomap.tier_${tier}`) }))}
        ariaLabel={t("repomap.tier")}
        placeholder={t("repomap.tier_other")}
      />
    </div>
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

/** Drawer detail tab with no repo selected. */
function EmptyProfileBody() {
  const { t } = useTranslation();
  return (
    <div className="flex h-full flex-col items-center justify-center px-5 text-center">
      <CircleDashed size={22} className="text-ink-faint" />
      <p className="mt-3 text-[13px] font-medium text-ink">{t("repomap.selectRepo")}</p>
      <p className="mt-1 text-[12px] leading-relaxed text-ink-faint">{t("repomap.selectRepoBody")}</p>
    </div>
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

/** The node's one-line summary as shown on a map CARD: read-only, clamped to 2 lines.
 *  (Editing lives only in the detail panel now — a card shouldn't be a tiny editor.) */
function NodeSummary({ profile }: { profile: RepoProfile }) {
  const { t } = useTranslation();
  return (
    <span
      className={cn(
        "min-w-0 break-words text-[11.5px] leading-snug",
        profile.summary ? "text-ink-muted" : "text-ink-faint italic",
      )}
      style={{ display: "-webkit-box", WebkitLineClamp: 2, WebkitBoxOrient: "vertical", overflow: "hidden" }}
    >
      {profile.summary || t("repomap.addSummary")}
    </span>
  );
}

/** The repo's one-line summary in the DETAIL panel: read in place, edited in a modal
 *  via the pencil. A multi-line textarea + Save/Cancel — more deliberate than the old
 *  inline single-line input. A user edit pins the summary (the wording teaches the map). */
function EditableSummary({ profile }: { profile: RepoProfile }) {
  const { editRepoSummary } = useStore();
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [text, setText] = useState(profile.summary);
  const [saving, setSaving] = useState(false);
  const taRef = useRef<HTMLTextAreaElement>(null);

  // Put the caret at the END when the modal opens. Deferred past Radix's
  // open-autofocus (which otherwise lands the caret at position 0), so the user
  // continues editing from the tail of the existing text.
  useEffect(() => {
    if (!open) return;
    const id = requestAnimationFrame(() => {
      const el = taRef.current;
      if (!el) return;
      el.focus();
      const end = el.value.length;
      el.setSelectionRange(end, end);
    });
    return () => cancelAnimationFrame(id);
  }, [open]);

  async function save() {
    const next = text.trim();
    if (next === profile.summary) {
      setOpen(false);
      return;
    }
    setSaving(true);
    try {
      await editRepoSummary(profile.repo_id, next);
      setOpen(false);
    } finally {
      setSaving(false);
    }
  }

  return (
    <>
      <div className="group/sum flex items-start gap-1.5">
        <span
          className={cn(
            "min-w-0 flex-1 break-words text-[12.5px] leading-relaxed",
            profile.summary ? "text-ink" : "text-ink-faint italic",
          )}
        >
          {profile.summary || t("repomap.addSummary")}
        </span>
        {ownsSummary(profile.source) && (
          <span className="mt-px shrink-0 rounded bg-brand-ghost px-1 py-px text-[9px] font-medium text-brand">
            {t("repomap.yours")}
          </span>
        )}
        <button
          onClick={() => {
            setText(profile.summary);
            setOpen(true);
          }}
          title={t("repomap.editHint")}
          aria-label={t("repomap.editHint")}
          className="grid h-6 w-6 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint opacity-0 transition-[opacity,color,background-color] hover:bg-raised hover:text-ink group-hover/sum:opacity-100"
        >
          <Pencil size={12} />
        </button>
      </div>

      <Dialog open={open} onOpenChange={(o) => !saving && setOpen(o)}>
        <DialogContent title={t("repomap.editSummaryTitle")}>
          <textarea
            ref={taRef}
            rows={2}
            value={text}
            onChange={(e) => setText(e.currentTarget.value)}
            onKeyDown={(e) => {
              // ⌘/Ctrl+Enter saves; plain Enter inserts a newline (it's multi-line now).
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) void save();
            }}
            placeholder={t("repomap.summaryPlaceholder")}
            className="w-full resize-none rounded-[var(--radius-md)] border border-border bg-bg px-2.5 py-2.5 text-[13px] leading-tight text-ink outline-none focus:border-brand/60"
          />
          <div className="mt-4 flex justify-end gap-2">
            <Button variant="ghost" disabled={saving} onClick={() => setOpen(false)}>
              {t("common.cancel")}
            </Button>
            <Button variant="primary" disabled={saving} onClick={() => void save()}>
              {t("common.save")}
            </Button>
          </div>
        </DialogContent>
      </Dialog>
    </>
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
