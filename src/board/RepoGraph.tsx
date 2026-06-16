import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  AppWindow,
  Boxes,
  CircleDashed,
  FileText,
  GitBranch,
  Maximize2,
  MessagesSquare,
  Minus,
  Package,
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
import type { RepoEdge, RepoProfile } from "../lib/types";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { Button } from "../components/ui/Button";
import { cn } from "../lib/cn";

const ROLE_ICON: Record<string, ComponentType<LucideProps>> = {
  service: Server,
  app: AppWindow,
  library: Package,
  infra: Boxes,
  docs: FileText,
  unknown: CircleDashed,
};

const NODE_W = 248;
const NODE_H = 108;
const COL_GAP = 82;
const ROW_GAP = 18;
const PAD = 18;
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
  const { repoProfiles, repoEdges, reprofileRepo, reanalyzeDeps, openCuratorChat } = useStore();
  const [analyzing, setAnalyzing] = useState(false);
  const { t } = useTranslation();
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [profileOpen, setProfileOpen] = useState(true);
  const seededSelection = useRef(false);

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
      // zoom proportional to the scroll delta so a trackpad's many small events
      // stay gentle; clamp the per-event delta so a mouse wheel can't jump.
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
    // let node interactions (re-profile) and the zoom controls through; only the background pans
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
              <marker
                id="weft-arrow-active"
                viewBox="0 0 8 8"
                refX="6"
                refY="4"
                markerWidth="6"
                markerHeight="6"
                orient="auto-start-reverse"
              >
                <path d="M0 0 L8 4 L0 8 z" className="fill-brand" />
              </marker>
            </defs>
            {repoEdges.map((e, i) => {
              const dependent = layout.pos.get(e.from);
              const dependency = layout.pos.get(e.to);
              if (!dependent || !dependency) return null;
              const x1 = dependency.x + NODE_W;
              const y1 = dependency.y + NODE_H / 2;
              const x2 = dependent.x;
              const y2 = dependent.y + NODE_H / 2;
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
            const Icon = ROLE_ICON[p.role] ?? CircleDashed;
            const dependents = repoEdges.filter((e) => e.to === p.repo_id).length;
            const core = dependents >= 2;
            const selected = selectedId === p.repo_id;
            return (
              <div
                key={p.repo_id}
                data-repo-node
                onClick={() => {
                  setSelectedId(p.repo_id);
                  setProfileOpen(true);
                }}
                className={cn(
                  "group absolute flex flex-col gap-2 overflow-hidden rounded-[var(--radius-md)] border bg-surface px-3 py-2.5 text-left transition-[transform,border-color,background-color] hover:-translate-y-px",
                  selected
                    ? "border-brand/60 bg-brand-ghost/60"
                    : core
                      ? "border-accent/50"
                      : "border-border hover:border-border-strong",
                )}
                style={{ left: pt.x, top: pt.y, width: NODE_W, height: NODE_H }}
              >
                <div className="flex items-center gap-1.5">
                  <span className="grid h-5 w-5 shrink-0 place-items-center rounded bg-raised">
                    <Icon size={12} className={selected ? "text-brand" : "text-ink-muted"} />
                  </span>
                  <span
                    title={p.repo_name}
                    className="min-w-0 flex-1 truncate text-[13.5px] font-semibold text-ink"
                  >
                    {p.repo_name}
                  </span>
                  {p.stale && (
                    <span
                      title={t("repomap.staleTitle")}
                      className="h-1.5 w-1.5 shrink-0 rounded-full bg-waiting"
                    />
                  )}
                  <button
                    onClick={(e) => {
                      e.stopPropagation();
                      void reprofileRepo(p.repo_id);
                    }}
                    aria-label={t("repomap.reprofile")}
                    title={t("repomap.reprofile")}
                    className="grid h-5 w-5 shrink-0 place-items-center rounded text-ink-faint opacity-0 transition-opacity hover:bg-brand-ghost hover:text-ink group-hover:opacity-100"
                  >
                    <RefreshCw size={11} />
                  </button>
                </div>

                <NodeBadges role={p.role} stack={p.stack} core={core} dependents={dependents} />

                <NodeSummary profile={p} />
              </div>
            );
          })}
        </div>

        <div className="pointer-events-none absolute inset-x-4 bottom-4 flex items-end justify-between gap-3">
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
  const Icon = ROLE_ICON[profile.role] ?? CircleDashed;

  return (
    <aside className="flex w-[320px] shrink-0 flex-col overflow-hidden rounded-[var(--radius-lg)] border border-border bg-surface">
      <div className="border-b border-border px-4 py-3">
        <div className="flex min-h-10 items-center gap-2.5">
          <span className="grid h-8 w-8 shrink-0 place-items-center rounded-[var(--radius-md)] bg-brand-ghost">
            <Icon size={16} className="text-brand" />
          </span>
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <h2 className="truncate font-mono text-[16px] font-semibold text-ink">
                {profile.repo_name}
              </h2>
              <span className="shrink-0 rounded-full border border-border bg-bg px-2 py-0.5 text-[11px] text-ink-muted">
                {t(`repomap.role_${profile.role}`, profile.role)}
              </span>
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
        <ProfileSection title={t("repomap.oneLine")}>
          <NodeSummary profile={profile} />
        </ProfileSection>

        <div className="grid grid-cols-2 gap-3">
          <ProfileSection title={t("repomap.stack")}>
            <ChipList values={profile.stack} empty={t("repomap.none")} mono />
          </ProfileSection>
          <ProfileSection title={t("repomap.source")}>
            <span className="text-[13px] text-ink-muted">
              {t(`repomap.source_${profile.source}`, profile.source)}
            </span>
          </ProfileSection>
        </div>

        <ProfileSection title={t("repomap.published")}>
          <ChipList values={profile.published} empty={t("repomap.none")} mono />
        </ProfileSection>

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
          <p className="text-[13px] leading-relaxed text-ink-muted">
            {t("repomap.deleteRepoBody")}
          </p>
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
      <p className="mt-1 text-[12px] leading-relaxed text-ink-faint">
        {t("repomap.selectRepoBody")}
      </p>
    </aside>
  );
}

function NodeBadges({
  role,
  stack,
  core,
  dependents,
}: {
  role: string;
  stack: string[];
  core: boolean;
  dependents: number;
}) {
  const { t } = useTranslation();
  const visibleStack = stack.slice(0, 2);
  const hiddenStack = stack.slice(2);
  const roleLabel = t(`repomap.role_${role}`, role);

  return (
    <div className="flex min-h-[20px] min-w-0 items-center gap-1 overflow-hidden">
      <MiniPill title={roleLabel} rounded>
        {roleLabel}
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

function ProfileSection({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
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
          {/* Non-"lib" kinds are agent-inferred runtime/infra links — badge them. */}
          {edge.kind && edge.kind !== "lib" && (
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
  const { editRepoProfile } = useStore();
  const { t } = useTranslation();
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(profile.summary);

  async function save() {
    setEditing(false);
    const next = text.trim();
    if (next === profile.summary) return;
    await editRepoProfile(profile.repo_id, next, profile.role);
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
      {profile.source === "user" && (
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
