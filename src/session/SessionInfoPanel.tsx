import { useMemo, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { X, ChevronRight, ChevronDown, RefreshCw } from "lucide-react";
import type { SessionMeta, EnabledSkill, Direction } from "../lib/types";
import { ToolIcon, toolFullName } from "../components/ToolIcon";

/**
 * 常驻右栏「会话信息」:Context(token + %,不含花费)、Sub-tasks、Skills、MCP。
 * Skills/MCP/Sub-tasks 共享同一套渐进披露(<Section> 折叠头 + <OverflowList>
 * head+show-more):Skills/MCP 头可整段折叠,Sub-tasks 头常驻(最相关,不可隐藏);
 * 三者都用同一个 head + "Show N more" 控件封长列表。纯展示——数据由 store 的
 * leadMeta/workerMeta + workspaceSkills + directionsByThread 喂。
 */
export function SessionInfoPanel({
  meta,
  skills,
  subtasks,
  onClose,
  onReload,
  busy,
}: {
  meta: SessionMeta | undefined;
  skills: EnabledSkill[];
  /** 该 thread 已创建的子任务(lead 专用;worker 不传 → 不渲染该段)。 */
  subtasks?: Direction[];
  onClose: () => void;
  /** 重载会话:复用静默 re-spawn,拾取新加的 MCP / skill。 */
  onReload?: () => void;
  /** turn 进行中:重载灰掉(re-spawn 在下次 send 生效)。 */
  busy?: boolean;
}) {
  const { t } = useTranslation();
  const ct = meta?.contextTokens;
  const win = meta?.window;
  const pct = ct != null && win ? Math.min(100, Math.round((ct / win) * 100)) : null;

  // Workspace skills (`skills`) ∪ engine skills (`meta.engineSkills`), deduped by
  // name (workspace wins).
  const allSkills = useMemo(() => {
    const byName = new Map<string, { name: string; description: string }>();
    for (const s of skills) byName.set(s.name, { name: s.name, description: s.description });
    for (const s of meta?.engineSkills ?? []) {
      if (!byName.has(s.name)) byName.set(s.name, s);
    }
    return [...byName.values()];
  }, [skills, meta?.engineSkills]);

  // Newest-first; the overflow list shows the head and folds the rest.
  // created_at is a Unix-seconds string (store `now()`), not RFC3339 — new
  // Date("1718700000") is Invalid Date, so parse numerically (ISO fallback).
  // Whole-second granularity + batch dispatch (planner::confirm) means a
  // proposal's directions share one timestamp, so break ties on descending id
  // (autoincrement → higher = inserted later = newer); otherwise the stable
  // sort keeps list_directions' ascending-id order and hides the newest.
  const sortedSubtasks = useMemo(
    () =>
      [...(subtasks ?? [])].sort((a, b) => {
        const dt = subtaskTime(b.created_at) - subtaskTime(a.created_at);
        return dt !== 0 ? dt : b.id - a.id;
      }),
    [subtasks],
  );

  const servers = meta?.mcpServers ?? [];

  return (
    <aside className="flex h-full w-[270px] shrink-0 flex-col overflow-hidden border-l border-border bg-bg">
      <header className="flex items-center gap-2 border-b border-border px-3 py-2">
        <span className="text-[12px] font-semibold text-ink">{t("sessionInfo.title")}</span>
        {onReload && (
          <button
            onClick={onReload}
            disabled={busy}
            title={t("sessionInfo.reloadHint")}
            aria-label={t("sessionInfo.reload")}
            className="ml-auto grid h-7 w-7 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink disabled:cursor-not-allowed disabled:opacity-40"
          >
            <RefreshCw size={14} />
          </button>
        )}
        <button
          onClick={onClose}
          aria-label={t("common.close")}
          className={`${onReload ? "" : "ml-auto "}grid h-7 w-7 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink`}
        >
          <X size={15} />
        </button>
      </header>

      {/* overflow-y:scroll keeps the (custom, space-taking) scrollbar track
          permanently reserved, so expanding a list never changes the content
          width — scrollbar-gutter alone wasn't reliably honored here. */}
      <div className="min-h-0 flex-1 overflow-y-scroll">
        {/* Context — stats, not a list; keeps its own bespoke layout. */}
        <section className="border-b border-border px-4 py-3">
          <div className="flex items-center">
            <span className="text-[11px] text-ink-faint">{t("sessionInfo.context")}</span>
            {pct != null && (
              <span className="ml-auto text-[11px] text-ink-muted">
                {t("sessionInfo.used", { pct })}
              </span>
            )}
          </div>
          {ct != null && (
            <>
              <div className="mt-1.5 flex items-baseline gap-1.5">
                <span className="text-[18px] font-medium text-ink">{ct.toLocaleString()}</span>
                <span className="text-[11px] text-ink-faint">{t("sessionInfo.tokens")}</span>
              </div>
              {pct != null && (
                <div className="mt-2 h-1 overflow-hidden rounded-full bg-surface">
                  <div className="h-full bg-brand" style={{ width: `${pct}%` }} />
                </div>
              )}
            </>
          )}
          {/* model·window 独立于 token usage 渲染:codex 的 token 走首条消息后的 usage
              事件,但 model/window 由 session_meta 立即提供。 */}
          {meta?.model && (
            <div className="mt-1.5 truncate font-mono text-[10.5px] text-ink-faint">
              {meta.model}
              {win ? ` · ${Math.round(win / 1000)}k` : ""}
              {meta.reasoningEffort ? ` · ${meta.reasoningEffort}` : ""}
            </div>
          )}
          {ct == null && !meta?.model && (
            <div className="mt-1.5 text-[11px] text-ink-faint">{t("sessionInfo.pending")}</div>
          )}
        </section>

        {/* Sub-tasks — created directions, newest first. Lead-only. The header
            stays put (most task-relevant → not hideable); the list caps at 3. */}
        {sortedSubtasks.length > 0 && (
          // Stable keys keep each section's disclosure state attached to itself:
          // these same-type <Section> siblings would otherwise reconcile by
          // position, so inserting Sub-tasks (0→≥1 live) would migrate Skills'
          // and MCP's useState to the wrong section.
          <Section key="subtasks" title={t("sessionInfo.subtasks")} count={sortedSubtasks.length}>
            <OverflowList
              items={sortedSubtasks}
              head={3}
              layout="rows"
              renderItem={(d) => <SubtaskRow key={d.id} direction={d} />}
            />
          </Section>
        )}

        {/* Skills — collapsible; chips cap at 10 (chips are dense). */}
        <Section
          key="skills"
          title={t("sessionInfo.skills")}
          count={allSkills.length}
          collapsible={allSkills.length > 0}
        >
          {allSkills.length === 0 ? (
            <div className="mt-1.5 text-[11px] text-ink-faint">{t("sessionInfo.noSkills")}</div>
          ) : (
            <OverflowList
              items={allSkills}
              head={10}
              layout="wrap"
              renderItem={(s) => (
                <span
                  key={s.name}
                  title={s.description}
                  className="rounded-[var(--radius-sm)] border border-border bg-surface px-2 py-0.5 text-[11.5px] text-ink"
                >
                  {s.name}
                </span>
              )}
            />
          )}
        </Section>

        {/* MCP — collapsible; servers cap at 3, each row expands its tools. */}
        <Section key="mcp" title={t("sessionInfo.mcp")} count={servers.length} collapsible={servers.length > 0}>
          {servers.length > 0 ? (
            <OverflowList
              items={servers}
              head={3}
              layout="rows"
              renderItem={(s) => (
                <McpRow key={s.name} name={s.name} status={s.status} tools={s.tools} />
              )}
            />
          ) : (
            <div className="mt-1.5 text-[11px] text-ink-faint">{t("sessionInfo.pending")}</div>
          )}
        </Section>
      </div>
    </aside>
  );
}

/**
 * A panel section: label + count header over a body. When `collapsible`, the
 * header is a button that folds the whole body (chevron, grid-rows animation);
 * otherwise it's a static label and the body always shows. Default expanded.
 */
function Section({
  title,
  count,
  collapsible = false,
  children,
}: {
  title: string;
  count?: number;
  collapsible?: boolean;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(true);
  const header = (
    <>
      <span className="text-[11px] text-ink-faint">{title}</span>
      {count != null && <span className="ml-auto text-[11px] text-ink-faint">{count}</span>}
      {collapsible &&
        (open ? (
          <ChevronDown size={13} className="ml-1 text-ink-faint" />
        ) : (
          <ChevronRight size={13} className="ml-1 text-ink-faint" />
        ))}
    </>
  );
  return (
    <section className="border-b border-border px-4 py-3">
      {collapsible ? (
        <button type="button" onClick={() => setOpen((v) => !v)} className="flex w-full items-center">
          {header}
        </button>
      ) : (
        <div className="flex items-center">{header}</div>
      )}
      {collapsible ? (
        // grid-rows 0fr→1fr animates the body open/closed; inner overflow-hidden
        // clips it (margins included) mid-anim.
        <div
          className={`grid transition-[grid-template-rows] duration-200 ease-out ${
            open ? "grid-rows-[1fr]" : "grid-rows-[0fr]"
          }`}
        >
          {/* `inert` when collapsed: body stays mounted for the animation but
              leaves the tab order / a11y tree (clipped content isn't focusable). */}
          <div className="overflow-hidden" inert={!open}>
            {children}
          </div>
        </div>
      ) : (
        children
      )}
    </section>
  );
}

/**
 * Shows the first `head` items, then folds the rest behind a shared
 * "Show N more / Show less" toggle (grid-rows animation). `layout` picks the
 * container: stacked rows or wrapping chips.
 */
function OverflowList<T>({
  items,
  head,
  layout,
  renderItem,
}: {
  items: T[];
  head: number;
  layout: "rows" | "wrap";
  renderItem: (item: T) => ReactNode;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const container = layout === "wrap" ? "flex flex-wrap gap-1.5" : "flex flex-col gap-0.5";
  const rest = items.slice(head);
  return (
    <>
      <div className={`mt-1.5 ${container}`}>{items.slice(0, head).map(renderItem)}</div>
      {rest.length > 0 && (
        <>
          <div
            className={`grid transition-[grid-template-rows] duration-200 ease-out ${
              open ? "grid-rows-[1fr]" : "grid-rows-[0fr]"
            }`}
          >
            {/* `inert` when closed: the rows stay mounted (for the collapse
                animation) but leave the tab order / a11y tree, so keyboard users
                can't focus hidden interactive rows (e.g. MCP server buttons). */}
            <div className="overflow-hidden" inert={!open}>
              <div className={`${container} ${layout === "wrap" ? "mt-1.5" : "pt-0.5"}`}>
                {rest.map(renderItem)}
              </div>
            </div>
          </div>
          <button
            type="button"
            onClick={() => setOpen((v) => !v)}
            className="mt-1.5 flex w-full items-center gap-1 px-1.5 text-[11px] text-ink-faint transition-colors hover:text-ink"
          >
            {open ? (
              <>
                <ChevronDown size={13} />
                {t("sessionInfo.showLess")}
              </>
            ) : (
              <>
                <ChevronRight size={13} />
                {t("sessionInfo.showMore", { count: rest.length })}
              </>
            )}
          </button>
        </>
      )}
    </>
  );
}

/** created_at → epoch for ordering. Store writes Unix seconds as a string;
 * tolerate an ISO value too. Unparseable → 0 (sinks to the bottom). */
function subtaskTime(createdAt: string): number {
  const secs = Number(createdAt);
  if (Number.isFinite(secs)) return secs;
  const ms = Date.parse(createdAt);
  return Number.isNaN(ms) ? 0 : ms;
}

/** Lifecycle status → dot color, mirroring the board columns. */
function subtaskDot(status: string): string {
  switch (status) {
    case "working":
      return "bg-running";
    case "review":
      return "bg-brand";
    case "done":
      return "bg-accent";
    default: // queued | planning
      return "bg-idle";
  }
}

// Display-only row (not interactive → no hover affordance): tool icon, name,
// status dot.
function SubtaskRow({ direction }: { direction: Direction }) {
  return (
    <div className="flex items-center gap-2 rounded-[var(--radius-sm)] px-1.5 py-1">
      <span
        title={toolFullName(direction.tool)}
        className="grid h-5 w-5 shrink-0 place-items-center rounded-[var(--radius-sm)] border border-border bg-bg text-ink-muted"
      >
        <ToolIcon tool={direction.tool} size={12} />
      </span>
      <span className="min-w-0 flex-1 truncate text-[12.5px] text-ink">{direction.name}</span>
      <span
        title={direction.status}
        className={`h-1.5 w-1.5 shrink-0 rounded-full ${subtaskDot(direction.status)}`}
      />
    </div>
  );
}

// Interactive row (click toggles the nested tool list → hover affordance):
// status dot, name, tool count.
function McpRow({ name, status, tools }: { name: string; status: string; tools: string[] }) {
  const [open, setOpen] = useState(true); // 默认展开
  const { t } = useTranslation();
  const dot =
    status === "connected" ? "bg-running" : status === "failed" ? "bg-danger" : "bg-idle";
  const hasTools = tools.length > 0;
  return (
    <div>
      <button
        onClick={() => hasTools && setOpen((v) => !v)}
        title={status}
        className="flex w-full items-center gap-2 rounded-[var(--radius-sm)] px-1.5 py-1 text-left hover:bg-surface"
      >
        <span className={`h-1.5 w-1.5 shrink-0 rounded-full ${dot}`} />
        <span className="min-w-0 flex-1 truncate text-[12.5px] text-ink">{name}</span>
        {hasTools && (
          <span className="shrink-0 text-[10.5px] text-ink-faint">
            {t("sessionInfo.tools", { count: tools.length })}
          </span>
        )}
        {hasTools &&
          (open ? (
            <ChevronDown size={14} className="shrink-0 text-ink-faint" />
          ) : (
            <ChevronRight size={14} className="shrink-0 text-ink-faint" />
          ))}
      </button>
      {hasTools && open && (
        <div className="flex flex-col">
          {tools.map((tool) => (
            <span
              key={tool}
              className="truncate py-0.5 pl-[22px] font-mono text-[11.5px] text-ink-muted"
            >
              {tool}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}
