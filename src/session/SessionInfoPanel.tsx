import { useMemo, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { X, ChevronRight, ChevronDown, RefreshCw } from "lucide-react";
import type { SessionMeta, EnabledSkill, Direction } from "../lib/types";
import { ToolIcon, toolFullName } from "../components/ToolIcon";

type NamedSkill = { name: string; description: string };

/**
 * 常驻右栏「会话信息」:Sub-tasks、Skills、MCP。Context(token/%/model)不在这里——
 * 它常驻 composer 工具条的 ContextGauge(ChatComposer),面板只管列表型信息。
 * Sub-tasks/MCP 是 <Section> 静态头 + <OverflowList>(head+show-more):头常驻只读,
 * 长列表用同一个 head + "Show N more" 控件折叠尾部。Skills 是两个独立口径的
 * <SkillGroup>(各自的头 + OverflowList)——workspace 静态注入 vs 引擎运行时探测,
 * 见该段内注释与 #108;运行时探测那组按 `tool` 门控(opencode 无此探测能力,
 * 见 {@link skillDiscoverySupported} 与 #114 review)。纯展示——数据由 store 的
 * leadMeta/workerMeta + workspaceSkills + directionsByThread 喂。
 */
export function SessionInfoPanel({
  meta,
  skills,
  tool,
  subtasks,
  onOpenSubtask,
  onClose,
  onReload,
  busy,
}: {
  meta: SessionMeta | undefined;
  skills: EnabledSkill[];
  /** lead_tool / ObserveRef.tool — gates the "Discovered" Skills group to
   *  engines that can actually report it (see {@link skillDiscoverySupported}).
   *  Omitted/undefined (tool identity not resolved yet) still renders. */
  tool?: string;
  /** 该 thread 已创建的子任务(lead 专用;worker 不传 → 不渲染该段)。 */
  subtasks?: Direction[];
  /** 点击子任务行 → 打开该 worker 会话面(lead 专用;不传 → 行保持静态)。 */
  onOpenSubtask?: (directionId: number) => void;
  onClose: () => void;
  /** 重载会话:复用静默 re-spawn,拾取新加的 MCP / skill。 */
  onReload?: () => void;
  /** turn 进行中:重载灰掉(re-spawn 在下次 send 生效)。 */
  busy?: boolean;
}) {
  const { t } = useTranslation();

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
        {/* Sub-tasks — created directions, newest first. Lead-only. The header
            stays put (most task-relevant → not hideable); the list caps at 3. */}
        {sortedSubtasks.length > 0 && (
          // Stable keys pin each section's nested disclosure state (OverflowList
          // show-more, McpRow tool-expand) to itself — else inserting Sub-tasks
          // (0→≥1) reconciles these same-type siblings by position and migrates
          // Skills'/MCP's state to the wrong section.
          <Section key="subtasks" title={t("sessionInfo.subtasks")} count={sortedSubtasks.length}>
            <OverflowList
              items={sortedSubtasks}
              head={3}
              layout="rows"
              renderItem={(d) => (
                <SubtaskRow key={d.id} direction={d} onOpen={onOpenSubtask} />
              )}
            />
          </Section>
        )}

        {/* Skills — two independently-sourced readings, kept apart rather than
            merged into one count (issue #108). `skills` is Weft's injected
            catalog for this workspace (policy, static — from workspace_skills).
            `meta.engineSkills` is what the engine actually found on disk in the
            session cwd at the last probe (ground truth, often a superset — e.g.
            pre-existing/plugin skills outside Weft's catalog). A dedup-merge of
            the two used to back the section's single count, which visibly
            jumped (e.g. 41→79) the moment the turn-triggered engine probe
            resolved after the workspace fetch. Each group now owns its count. */}
        <Section key="skills" title={t("sessionInfo.skills")}>
          <div className="mt-1.5">
            <SkillGroup
              label={t("sessionInfo.skillsInjected")}
              hint={t("sessionInfo.skillsInjectedHint")}
              skills={skills}
              emptyText={t("sessionInfo.noSkills")}
            />
            {/* opencode has no discovery probe at all (session_meta.rs's
                gather_opencode never sets `skills`) — engineSkills would stay
                `undefined` forever, so the group would be stuck reading
                "pending" turn after turn instead of ever resolving. That's not
                the same as "not probed yet"; hide the reading entirely rather
                than show a promise that can't be kept (PR #114 review). */}
            {skillDiscoverySupported(tool) && (
              <div className="mt-3">
                <SkillGroup
                  label={t("sessionInfo.skillsDiscovered")}
                  hint={t("sessionInfo.skillsDiscoveredHint")}
                  skills={meta?.engineSkills}
                  emptyText={t("sessionInfo.noEngineSkills")}
                  pendingText={t("sessionInfo.pending")}
                />
              </div>
            )}
          </div>
        </Section>

        {/* MCP — servers cap at 3, each row expands its tools. */}
        <Section key="mcp" title={t("sessionInfo.mcp")} count={servers.length}>
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

/** A panel section: a static label + count header over a body that always shows. */
function Section({
  title,
  count,
  children,
}: {
  title: string;
  count?: number;
  children: ReactNode;
}) {
  return (
    <section className="border-b border-border px-4 py-3">
      <div className="flex items-center">
        <span className="text-[11px] text-ink-faint">{title}</span>
        {count != null && <span className="ml-auto text-[11px] text-ink-faint">{count}</span>}
      </div>
      {children}
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
            // No left inset; the chevron's -ml-1 cancels the lucide glyph's
            // internal whitespace so the visible arrow lines up with the section
            // title and the row dots/icons at the content edge.
            className="mt-1.5 flex w-full items-center gap-1 text-[11px] text-ink-faint transition-colors hover:text-ink"
          >
            {open ? (
              <>
                <ChevronDown size={13} className="-ml-1" />
                {t("sessionInfo.showLess")}
              </>
            ) : (
              <>
                <ChevronRight size={13} className="-ml-1" />
                {t("sessionInfo.showMore", { count: rest.length })}
              </>
            )}
          </button>
        </>
      )}
    </>
  );
}

/** Whether this session's engine has ANY mechanism to report which skills it
 *  actually loaded — mirrors `session_meta::gather`'s dispatch in
 *  `src-tauri/src/session_meta.rs`: claude scans the session cwd's
 *  `.claude/skills`, codex has its own skill discovery; opencode has no
 *  equivalent probe, so `gather_opencode` never sets `skills` and
 *  `meta.engineSkills` would stay `undefined` for the life of the session —
 *  not "pending", just permanently unavailable. `tool` unresolved
 *  (`undefined`, e.g. the worker surface before its session lookup lands)
 *  still counts as supported: the session_meta effects that would populate
 *  `engineSkills` already gate on the tool being known first (see
 *  LeadTab/WorkerConversation), so there's no real window where this default
 *  would show a reading that never arrives. */
function skillDiscoverySupported(tool: string | undefined): boolean {
  return tool !== "opencode";
}

/** One Skills reading's tri-state: `undefined` means no authoritative result
 *  has landed yet (the initial value, or every probe so far has failed —
 *  `sessionMeta.ts`'s merges keep this as `undefined`/prev rather than ever
 *  synthesizing a `[]`), `[]` means a probe DID land and confirmed zero, and a
 *  non-empty array is the actual list. Modeled as one discriminated value
 *  (instead of re-deriving `isPending`/`isEmpty` booleans at each call site)
 *  so "no signal yet" can never be silently displayed as "confirmed zero". */
type SkillReadingState = "pending" | "empty" | "list";

function skillReadingState(skills: unknown[] | undefined): SkillReadingState {
  if (skills == null) return "pending";
  return skills.length === 0 ? "empty" : "list";
}

/** Skills section body for one reading: an eyebrow label + its own count, over
 *  chips (head 10, dense) or an empty/pending hint. Exhaustive over
 *  `SkillReadingState` via {@link SkillGroupBody} rather than a nested ternary. */
function SkillGroup<T extends NamedSkill>({
  label,
  hint,
  skills,
  emptyText,
  pendingText = emptyText,
}: {
  label: string;
  hint: string;
  skills: T[] | undefined;
  /** Shown when the reading is authoritative and empty. */
  emptyText: string;
  /** Shown when the reading hasn't landed yet. Defaults to `emptyText` for
   *  readings that have no pending state (e.g. the injected list, always a
   *  concrete — if possibly not-yet-fetched-once — array). */
  pendingText?: string;
}) {
  const state = skillReadingState(skills);
  const list = skills ?? [];
  return (
    <div>
      <div className="flex items-center" title={hint}>
        <span className="text-[10.5px] font-medium uppercase tracking-wide text-ink-faint">
          {label}
        </span>
        {state !== "pending" && (
          <span className="ml-auto text-[10.5px] text-ink-faint">{list.length}</span>
        )}
      </div>
      <SkillGroupBody state={state} list={list} pendingText={pendingText} emptyText={emptyText} />
    </div>
  );
}

function SkillGroupBody<T extends NamedSkill>({
  state,
  list,
  pendingText,
  emptyText,
}: {
  state: SkillReadingState;
  list: T[];
  pendingText: string;
  emptyText: string;
}) {
  if (state === "list") {
    return (
      <OverflowList
        items={list}
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
    );
  }
  const text: Record<Exclude<SkillReadingState, "list">, string> = { pending: pendingText, empty: emptyText };
  return <div className="mt-1.5 text-[11px] text-ink-faint">{text[state]}</div>;
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

/** MCP server connection status → dot color. */
function mcpDot(status: string): string {
  if (status === "connected") return "bg-running";
  if (status === "failed") return "bg-danger";
  return "bg-idle";
}

// Display-only row (not interactive → no hover affordance): tool icon, name,
// status dot.
function SubtaskRow({
  direction,
  onOpen,
}: {
  direction: Direction;
  onOpen?: (directionId: number) => void;
}) {
  // -mx-1.5 mirrors McpRow: the tool icon lines up with the section title
  // (and the show-more chevron) at the content edge.
  const rowClass = "-mx-1.5 flex items-center gap-2 rounded-[var(--radius-sm)] px-1.5 py-1";
  const body = (
    <>
      <span
        title={toolFullName(direction.tool)}
        className="grid h-5 w-5 shrink-0 place-items-center rounded-[var(--radius-sm)] border border-border bg-bg text-ink-muted"
      >
        <ToolIcon tool={direction.tool} size={12} />
      </span>
      <span className="min-w-0 flex-1 truncate text-left text-[12.5px] text-ink">
        {direction.name}
      </span>
      <span
        title={direction.status}
        className={`h-1.5 w-1.5 shrink-0 rounded-full ${subtaskDot(direction.status)}`}
      />
    </>
  );
  if (!onOpen) {
    return <div className={rowClass}>{body}</div>;
  }
  // Interactive: click opens the direction's worker surface (McpRow's hover
  // affordance) — the panel row used to be the one task entry that led nowhere.
  return (
    <button
      type="button"
      onClick={() => onOpen(direction.id)}
      className={`${rowClass} w-[calc(100%+0.75rem)] transition-colors hover:bg-surface`}
    >
      {body}
    </button>
  );
}

// Interactive row (click toggles the nested tool list → hover affordance):
// status dot, name, tool count.
function McpRow({ name, status, tools }: { name: string; status: string; tools: string[] }) {
  const [open, setOpen] = useState(true); // 默认展开
  const { t } = useTranslation();
  const dot = mcpDot(status);
  const hasTools = tools.length > 0;
  return (
    // -mx-1.5 pulls the row out so the status dot lines up with the section
    // title; the button keeps px-1.5 so the hover pill stays inset.
    <div className="-mx-1.5">
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
