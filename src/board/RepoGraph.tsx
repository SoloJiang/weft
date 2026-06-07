import { useMemo } from "react";
import {
  AppWindow,
  Boxes,
  CircleDashed,
  FileText,
  Package,
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

const NODE_W = 168;
const NODE_H = 56;
const COL_GAP = 96;
const ROW_GAP = 24;
const PAD = 16;

/**
 * A visual dependency graph of the workspace repos: nodes laid out in columns by
 * dependency depth (foundational libs left, top-level apps right), edges drawn
 * dependent → dependency. Pure SVG + absolutely-positioned node cards.
 */
export function RepoGraph() {
  const { repoProfiles, repoEdges } = useStore();

  const layout = useMemo(() => {
    const ids = repoProfiles.map((p) => p.repo_id);
    const depsOf = (id: number) =>
      repoEdges.filter((e) => e.from === id).map((e) => e.to).filter((t) => ids.includes(t));
    const memo = new Map<number, number>();
    const depth = (id: number, seen = new Set<number>()): number => {
      const m = memo.get(id);
      if (m != null) return m;
      if (seen.has(id)) return 0; // cycle guard
      seen.add(id);
      const ds = depsOf(id);
      const d = ds.length === 0 ? 0 : 1 + Math.max(...ds.map((t) => depth(t, seen)));
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
      // vertically center each column within the tallest column
      const offset = ((maxRows - col.length) * (NODE_H + ROW_GAP)) / 2;
      col.forEach((id, i) => {
        pos.set(id, { x: PAD + d * (NODE_W + COL_GAP), y: PAD + offset + i * (NODE_H + ROW_GAP) });
      });
    }
    const width = PAD * 2 + (maxDepth + 1) * NODE_W + maxDepth * COL_GAP;
    const height = PAD * 2 + maxRows * (NODE_H + ROW_GAP) - ROW_GAP;
    return { pos, width, height };
  }, [repoProfiles, repoEdges]);

  if (repoProfiles.length === 0) return null;

  const usedByCount = (id: number) => repoEdges.filter((e) => e.to === id).length;

  return (
    <div className="overflow-x-auto px-5 py-5">
      <div className="relative mx-auto" style={{ width: layout.width, height: layout.height }}>
        <svg
          className="absolute inset-0"
          width={layout.width}
          height={layout.height}
          fill="none"
        >
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
            // from (dependent) left edge → to (dependency) right edge
            const x1 = a.x;
            const y1 = a.y + NODE_H / 2;
            const x2 = b.x + NODE_W;
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
          const core = usedByCount(p.repo_id) >= 2;
          return (
            <div
              key={p.repo_id}
              className={cn(
                "absolute flex flex-col justify-center gap-0.5 rounded-[var(--radius-lg)] border bg-surface px-3",
                core ? "border-accent/50" : "border-border",
              )}
              style={{ left: pt.x, top: pt.y, width: NODE_W, height: NODE_H }}
            >
              <div className="flex items-center gap-1.5">
                <span className="grid h-5 w-5 shrink-0 place-items-center rounded bg-raised">
                  <Icon size={12} className="text-ink-muted" />
                </span>
                <span className="truncate text-[12.5px] font-medium text-ink">
                  {p.repo_name}
                </span>
                {core && (
                  <span className="ml-auto h-1.5 w-1.5 shrink-0 rounded-full bg-accent" />
                )}
              </div>
              {p.summary && (
                <span className="truncate pl-[26px] text-[10.5px] text-ink-faint">
                  {p.summary}
                </span>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
