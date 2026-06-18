import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { X, ChevronRight, ChevronDown, RefreshCw } from "lucide-react";
import type { SessionMeta, EnabledSkill, Direction } from "../lib/types";
import { ToolIcon, toolFullName } from "../components/ToolIcon";

/**
 * 常驻右栏「会话信息」:Context(token + %,不含花费)、Skills、MCP(server + 状态,
 * claude 可展开看 tool;codex/opencode 只列 server)。纯展示——数据由 store 的
 * leadMeta/workerMeta + workspaceSkills 喂;和 diff 互斥由宿主的 rail 状态驱动。
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

  // Collapsible skills: a long list (codex exposes dozens) would bury the MCP
  // section, so default to expanded only when there are few; null = use default.
  const [skillsOpen, setSkillsOpen] = useState<boolean | null>(null);
  const skillsExpanded = skillsOpen ?? allSkills.length <= 12;

  // Newest-first; the panel shows the top 3 and folds the rest behind a toggle.
  // direction.created_at is a Unix-seconds string (store `now()`), not RFC3339 —
  // new Date("1718700000") is Invalid Date, so parse numerically (ISO fallback).
  const sortedSubtasks = useMemo(
    () => [...(subtasks ?? [])].sort((a, b) => subtaskTime(b.created_at) - subtaskTime(a.created_at)),
    [subtasks],
  );
  const [subtasksOpen, setSubtasksOpen] = useState(false);
  const SUBTASK_HEAD = 3;

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
          permanently reserved, so expanding the skills list never changes the
          content width — scrollbar-gutter alone wasn't reliably honored here. */}
      <div className="min-h-0 flex-1 overflow-y-scroll">
        {/* Context */}
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

        {/* Sub-tasks — created directions for this thread, newest first. Only the
            lead passes them; the top 3 stay visible, the rest fold away. */}
        {sortedSubtasks.length > 0 && (
          <section className="border-b border-border px-4 py-3">
            <div className="flex items-center">
              <span className="text-[11px] text-ink-faint">{t("sessionInfo.subtasks")}</span>
              <span className="ml-auto text-[11px] text-ink-faint">{sortedSubtasks.length}</span>
            </div>
            <div className="mt-1.5 flex flex-col gap-0.5">
              {sortedSubtasks.slice(0, SUBTASK_HEAD).map((d) => (
                <SubtaskRow key={d.id} direction={d} />
              ))}
            </div>
            {sortedSubtasks.length > SUBTASK_HEAD && (
              <>
                {/* grid-rows 0fr→1fr animates the overflow open/closed; the inner
                    overflow-hidden clips the rows mid-anim. */}
                <div
                  className={`grid transition-[grid-template-rows] duration-200 ease-out ${
                    subtasksOpen ? "grid-rows-[1fr]" : "grid-rows-[0fr]"
                  }`}
                >
                  <div className="overflow-hidden">
                    <div className="flex flex-col gap-0.5 pt-0.5">
                      {sortedSubtasks.slice(SUBTASK_HEAD).map((d) => (
                        <SubtaskRow key={d.id} direction={d} />
                      ))}
                    </div>
                  </div>
                </div>
                <button
                  type="button"
                  onClick={() => setSubtasksOpen((v) => !v)}
                  className="mt-1.5 flex w-full items-center gap-1 px-1.5 text-[11px] text-ink-faint transition-colors hover:text-ink"
                >
                  {subtasksOpen ? (
                    <>
                      <ChevronDown size={13} />
                      {t("sessionInfo.showLess")}
                    </>
                  ) : (
                    <>
                      <ChevronRight size={13} />
                      {t("sessionInfo.moreSubtasks", {
                        count: sortedSubtasks.length - SUBTASK_HEAD,
                      })}
                    </>
                  )}
                </button>
              </>
            )}
          </section>
        )}

        {/* Skills */}
        <section className="border-b border-border px-4 py-3">
          <button
            type="button"
            onClick={() => allSkills.length > 0 && setSkillsOpen(!skillsExpanded)}
            className="flex w-full items-center"
          >
            <span className="text-[11px] text-ink-faint">{t("sessionInfo.skills")}</span>
            <span className="ml-auto text-[11px] text-ink-faint">{allSkills.length}</span>
            {allSkills.length > 0 &&
              (skillsExpanded ? (
                <ChevronDown size={13} className="ml-1 text-ink-faint" />
              ) : (
                <ChevronRight size={13} className="ml-1 text-ink-faint" />
              ))}
          </button>
          {allSkills.length === 0 ? (
            <div className="mt-2 text-[11px] text-ink-faint">{t("sessionInfo.noSkills")}</div>
          ) : (
            // grid-rows 0fr→1fr animates the chip wrap open/closed without a fixed
            // height; the inner overflow-hidden clips (margin included) mid-anim.
            <div
              className={`grid transition-[grid-template-rows] duration-200 ease-out ${
                skillsExpanded ? "grid-rows-[1fr]" : "grid-rows-[0fr]"
              }`}
            >
              <div className="overflow-hidden">
                <div className="mt-2 flex flex-wrap gap-1.5">
                  {allSkills.map((s) => (
                    <span
                      key={s.name}
                      title={s.description}
                      className="rounded-[var(--radius-sm)] border border-border bg-surface px-2 py-0.5 text-[11.5px] text-ink"
                    >
                      {s.name}
                    </span>
                  ))}
                </div>
              </div>
            </div>
          )}
        </section>

        {/* MCP */}
        <section className="px-4 py-3">
          <div className="flex items-center">
            <span className="text-[11px] text-ink-faint">{t("sessionInfo.mcp")}</span>
            <span className="ml-auto text-[11px] text-ink-faint">
              {t("sessionInfo.servers", { count: meta?.mcpServers.length ?? 0 })}
            </span>
          </div>
          {meta && meta.mcpServers.length > 0 ? (
            <div className="mt-1.5 flex flex-col gap-0.5">
              {meta.mcpServers.map((s) => (
                <McpRow key={s.name} name={s.name} status={s.status} tools={s.tools} />
              ))}
            </div>
          ) : (
            <div className="mt-1.5 text-[11px] text-ink-faint">{t("sessionInfo.pending")}</div>
          )}
        </section>
      </div>
    </aside>
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
