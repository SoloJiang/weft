import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  AppWindow,
  Boxes,
  CircleDashed,
  FileText,
  Maximize2,
  Minus,
  Package,
  Plus,
  RefreshCw,
  Server,
  type LucideProps,
} from "lucide-react";
import type { ComponentType } from "react";
import { useStore } from "../state/store";
import { cn } from "../lib/cn";

const ROLE_ICON: Record<string, ComponentType<LucideProps>> = {
  service: Server,
  app: AppWindow,
  library: Package,
  infra: Boxes,
  docs: FileText,
  unknown: CircleDashed,
};

const NODE_W = 250;
const NODE_H = 116;
const COL_GAP = 112;
const ROW_GAP = 28;
const PAD = 24;
const MIN_Z = 0.35;
const MAX_Z = 2.5;
const clampZ = (z: number) => Math.min(MAX_Z, Math.max(MIN_Z, z));

/**
 * The repo map as a pan/zoom canvas — the whole Repos surface. Nodes are laid
 * out in columns by dependency depth (foundational libs left, top-level apps
 * right) and carry everything: role, stack, one-line summary, core flag,
 * re-profile. Edges are drawn dependent → dependency. Drag the background to
 * pan, scroll/buttons to zoom, fit to recenter.
 */
export function RepoGraph() {
  const { repoProfiles, repoEdges, reprofileRepo } = useStore();
  const { t } = useTranslation();

  const layout = useMemo(() => {
    const ids = repoProfiles.map((p) => p.repo_id);
    const depsOf = (id: number) =>
      repoEdges.filter((e) => e.from === id).map((e) => e.to).filter((to) => ids.includes(to));
    const memo = new Map<number, number>();
    const depth = (id: number, seen = new Set<number>()): number => {
      const m = memo.get(id);
      if (m != null) return m;
      if (seen.has(id)) return 0; // cycle guard
      seen.add(id);
      const ds = depsOf(id);
      const d = ds.length === 0 ? 0 : 1 + Math.max(...ds.map((to) => depth(to, seen)));
      memo.set(id, d);
      return d;
    };

    const cols = new Map<number, number[]>();
    for (const p of repoProfiles) {
      const d = depth(p.repo_id);
      const arr = cols.get(d) ?? [];
      arr.push(p.repo_id);
      cols.set(d, arr);
    }
    const maxDepth = Math.max(0, ...[...cols.keys()]);
    const maxRows = Math.max(1, ...[...cols.values()].map((a) => a.length));

    const pos = new Map<number, { x: number; y: number }>();
    for (let d = 0; d <= maxDepth; d++) {
      const col = cols.get(d) ?? [];
      const offset = ((maxRows - col.length) * (NODE_H + ROW_GAP)) / 2;
      col.forEach((id, i) => {
        pos.set(id, { x: PAD + d * (NODE_W + COL_GAP), y: PAD + offset + i * (NODE_H + ROW_GAP) });
      });
    }
    const width = PAD * 2 + (maxDepth + 1) * NODE_W + maxDepth * COL_GAP;
    const height = PAD * 2 + maxRows * (NODE_H + ROW_GAP) - ROW_GAP;
    return { pos, width, height };
  }, [repoProfiles, repoEdges]);

  const containerRef = useRef<HTMLDivElement>(null);
  const [zoom, setZoom] = useState(1);
  const [pan, setPan] = useState({ x: 0, y: 0 });
  const drag = useRef<{ x: number; y: number; px: number; py: number } | null>(null);

  const fit = useCallback(() => {
    const el = containerRef.current;
    if (!el) return;
    const cw = el.clientWidth;
    const ch = el.clientHeight;
    const z = clampZ(Math.min((cw - 96) / layout.width, (ch - 96) / layout.height, 1));
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
      zoomAt(e.clientX - rect.left, e.clientY - rect.top, e.deltaY < 0 ? 1.12 : 0.89);
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
    // let node interactions (re-profile) through; only the background pans
    if ((e.target as HTMLElement).closest("[data-repo-node]")) return;
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

  return (
    <div
      ref={containerRef}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={endDrag}
      onPointerLeave={endDrag}
      className="relative h-full w-full cursor-grab select-none overflow-hidden bg-bg [touch-action:none] active:cursor-grabbing"
    >
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
            <marker
              id="weft-arrow"
              viewBox="0 0 8 8"
              refX="6"
              refY="4"
              markerWidth="6"
              markerHeight="6"
              orient="auto-start-reverse"
            >
              <path d="M0 0 L8 4 L0 8 z" className="fill-border-strong" />
            </marker>
          </defs>
          {repoEdges.map((e, i) => {
            const a = layout.pos.get(e.from);
            const b = layout.pos.get(e.to);
            if (!a || !b) return null;
            const x1 = a.x; // dependent, left edge
            const y1 = a.y + NODE_H / 2;
            const x2 = b.x + NODE_W; // dependency, right edge
            const y2 = b.y + NODE_H / 2;
            const mx = (x1 + x2) / 2;
            return (
              <path
                key={i}
                d={`M ${x1} ${y1} C ${mx} ${y1}, ${mx} ${y2}, ${x2} ${y2}`}
                className="stroke-border-strong"
                strokeWidth={1.5}
                markerEnd="url(#weft-arrow)"
              />
            );
          })}
        </svg>

        {repoProfiles.map((p) => {
          const pt = layout.pos.get(p.repo_id);
          if (!pt) return null;
          const Icon = ROLE_ICON[p.role] ?? CircleDashed;
          const dependents = repoEdges.filter((e) => e.to === p.repo_id).length;
          const core = dependents >= 2;
          return (
            <div
              key={p.repo_id}
              data-repo-node
              className={cn(
                "group absolute flex flex-col gap-1.5 overflow-hidden rounded-[var(--radius-lg)] border bg-surface px-3 py-2.5 shadow-[0_1px_3px_rgba(0,0,0,0.06)]",
                core ? "border-accent/50" : "border-border",
              )}
              style={{ left: pt.x, top: pt.y, width: NODE_W, height: NODE_H }}
            >
              <div className="flex items-center gap-1.5">
                <span className="grid h-5 w-5 shrink-0 place-items-center rounded bg-raised">
                  <Icon size={12} className="text-ink-muted" />
                </span>
                <span className="min-w-0 flex-1 truncate text-[13px] font-semibold text-ink">
                  {p.repo_name}
                </span>
                {p.stale && (
                  <span
                    title={t("repomap.staleTitle")}
                    className="h-1.5 w-1.5 shrink-0 rounded-full bg-waiting"
                  />
                )}
                <button
                  onClick={() => void reprofileRepo(p.repo_id)}
                  aria-label={t("repomap.reprofile")}
                  title={t("repomap.reprofile")}
                  className="grid h-5 w-5 shrink-0 place-items-center rounded text-ink-faint opacity-0 transition-opacity hover:bg-brand-ghost hover:text-ink group-hover:opacity-100"
                >
                  <RefreshCw size={11} />
                </button>
              </div>

              <div className="flex flex-nowrap items-center gap-1 overflow-hidden">
                <span className="shrink-0 rounded-full bg-bg px-1.5 py-px text-[10px] text-ink-faint">
                  {t(`repomap.role_${p.role}`, p.role)}
                </span>
                {p.stack.slice(0, 3).map((s) => (
                  <span
                    key={s}
                    className="shrink-0 rounded bg-bg px-1.5 py-px font-mono text-[10px] text-ink-faint"
                  >
                    {s}
                  </span>
                ))}
                {core && (
                  <span
                    title={t("repomap.rippleTitle", { count: dependents })}
                    className="ml-auto shrink-0 rounded-full bg-accent-ghost px-1.5 py-px text-[10px] font-medium text-accent"
                  >
                    {t("repomap.coreDependents", { count: dependents })}
                  </span>
                )}
              </div>

              <p
                className={cn(
                  "text-[11.5px] leading-snug",
                  p.summary ? "text-ink-muted" : "text-ink-faint italic",
                )}
                style={{
                  display: "-webkit-box",
                  WebkitLineClamp: 2,
                  WebkitBoxOrient: "vertical",
                  overflow: "hidden",
                }}
              >
                {p.summary || t("repomap.addSummary")}
              </p>
            </div>
          );
        })}
      </div>

      {/* zoom controls */}
      <div className="absolute bottom-4 right-4 flex items-center gap-0.5 rounded-[var(--radius-md)] border border-border bg-raised p-1 shadow-[0_4px_16px_-6px_rgba(0,0,0,0.4)]">
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
