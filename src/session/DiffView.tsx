import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import {
  ChevronDown,
  ChevronRight,
  FileText,
  MessageSquarePlus,
  RefreshCw,
  Send,
} from "lucide-react";
import { api } from "../lib/api";
import type { WorktreeDiff } from "../lib/types";
import { cn } from "../lib/cn";
import { useClickOutside } from "../lib/useClickOutside";
import { Tooltip } from "../components/ui/Tooltip";

/**
 * Worker review surface: the worktree's net git diff (file stats + unified
 * patch), polled live, as collapsible per-file sections. Lets you see exactly
 * what the worker changed without dropping into the terminal. With `onAsk`,
 * any file header or diff line becomes an annotation: ask a question in place
 * and it lands in the worker's own conversation (waking it if needed).
 */
export function DiffView({
  cwd,
  directionId,
  onAsk,
}: {
  cwd: string;
  /** The task whose worktree this is — enables the "vs target" mode + editor. */
  directionId?: number | null;
  /** Deliver an annotation question to the responsible worker. */
  onAsk?: (text: string) => void;
}) {
  const { t } = useTranslation();
  const [diff, setDiff] = useState<WorktreeDiff | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [open, setOpen] = useState<Record<string, boolean>>({});
  const [touched, setTouched] = useState(false);
  /** The annotation being composed: a file, optionally pinned to one line. */
  const [ask, setAsk] = useState<{ path: string; line?: DiffLine } | null>(null);
  // "worktree" = working-tree changes vs HEAD (default). "target" = PR-style
  // diff vs the task's target branch. The target view needs to know the task.
  const canTarget = directionId != null;
  const [mode, setMode] = useState<"worktree" | "target">("worktree");
  const [tgt, setTgt] = useState<TargetMeta | null>(null);
  // Bumped to re-run the effect WITH a fresh origin/<target> fetch (manual
  // refresh + after a target edit; mode-enter already re-runs the effect).
  const [reload, setReload] = useState(0);
  const rootRef = useRef<HTMLDivElement>(null);
  // Pressing outside the diff closes an open AskBox. Scoped to the whole surface
  // (not the box) so clicking another line just retargets it, keeping the draft.
  useClickOutside(rootRef, ask != null, () => setAsk(null));

  const targetMode = mode === "target" && canTarget;

  useEffect(() => {
    let alive = true;
    // Fetch origin/<target> only on the first tick of a (re)entered target view;
    // the 3s poll afterwards recomputes against the cached ref (cheap, no network).
    let fresh = true;
    const tick = async () => {
      try {
        if (targetMode && directionId != null) {
          const d = await api.worktreeDiffTarget(cwd, directionId, fresh);
          if (alive) {
            setDiff({ files: d.files, patch: d.patch });
            setTgt({ target: d.target, defaultBranch: d.default_branch, resolved: d.resolved });
            setLoaded(true);
          }
        } else {
          const d = await api.worktreeDiff(cwd);
          if (alive) {
            setDiff(d);
            setLoaded(true);
          }
        }
      } catch {
        /* not ready */
      }
      fresh = false;
    };
    setLoaded(false);
    void tick();
    const h = setInterval(tick, 3000);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, [cwd, targetMode, directionId, reload]);

  const bodyByPath = useMemo(() => parsePatch(diff?.patch ?? ""), [diff?.patch]);

  const files = diff?.files ?? [];
  const isEmpty = loaded && files.length === 0;
  // Default: expand all when few files, collapse when many (until the user acts).
  const isOpen = (p: string) =>
    touched ? !!open[p] : files.length <= 4;
  const toggle = (p: string) => {
    setTouched(true);
    setOpen((m) => ({ ...m, [p]: !isOpen(p) }));
  };

  const totalAdded = files.reduce((s, f) => s + f.added, 0);
  const totalRemoved = files.reduce((s, f) => s + f.removed, 0);

  return (
    <div ref={rootRef} className="flex min-h-0 min-w-0 flex-1 flex-col overflow-y-auto">
      <div className="sticky top-0 z-10 flex flex-col gap-2 border-b border-border bg-bg/95 px-4 py-2.5 backdrop-blur">
        <div className="flex items-center gap-2 text-[11px] text-ink-faint">
          {canTarget && (
            <div className="flex items-center gap-0.5 rounded-[var(--radius-md)] border border-border p-0.5">
              <ModeBtn active={mode === "worktree"} onClick={() => setMode("worktree")}>
                {t("diff.modeWorktree")}
              </ModeBtn>
              <ModeBtn active={mode === "target"} onClick={() => setMode("target")}>
                {t("diff.modeTarget")}
              </ModeBtn>
            </div>
          )}
          <span className="ml-auto">{t("diff.filesChanged", { count: files.length })}</span>
          <span className="text-running">+{totalAdded}</span>
          <span className="text-danger">−{totalRemoved}</span>
        </div>
        {targetMode && (
          <TargetEditor
            directionId={directionId!}
            meta={tgt}
            onChanged={() => setReload((n) => n + 1)}
          />
        )}
      </div>

      {isEmpty ? (
        <div className="flex flex-1 items-center justify-center px-6 text-center">
          <p className="text-[12px] leading-relaxed text-ink-faint">
            {targetMode ? t("diff.emptyTarget") : t("diff.empty")}
          </p>
        </div>
      ) : (
      <div className="flex min-w-0 flex-col">
        {files.map((f) => {
          const body = bodyByPath[f.path];
          const expanded = isOpen(f.path);
          return (
            <div key={f.path} className="min-w-0 border-b border-border/60">
              <div className="group flex w-full items-center gap-2 px-3 py-2 transition-colors hover:bg-surface">
                <button
                  onClick={() => toggle(f.path)}
                  className="flex min-w-0 flex-1 items-center gap-2 text-left"
                >
                  {expanded ? (
                    <ChevronDown size={13} className="shrink-0 text-ink-faint" />
                  ) : (
                    <ChevronRight size={13} className="shrink-0 text-ink-faint" />
                  )}
                  <FileText size={12} className="shrink-0 text-ink-faint" />
                  <span className="truncate font-mono text-[12px] text-ink">{f.path}</span>
                </button>
                {onAsk && (
                  <Tooltip label={t("diff.ask")}>
                    <button
                      onClick={() => setAsk({ path: f.path })}
                      aria-label={t("diff.ask")}
                      className="grid h-6 w-6 shrink-0 place-items-center rounded text-ink-faint opacity-0 transition-opacity hover:bg-brand-ghost hover:text-ink group-hover:opacity-100"
                    >
                      <MessageSquarePlus size={12} />
                    </button>
                  </Tooltip>
                )}
                <span className="shrink-0 tabular-nums text-[11px]">
                  <span className="text-running">+{f.added}</span>{" "}
                  <span className="text-danger">−{f.removed}</span>
                </span>
              </div>
              {ask?.path === f.path && onAsk && (
                <AskBox
                  path={f.path}
                  line={ask.line}
                  onSend={(text) => {
                    onAsk(text);
                    setAsk(null);
                  }}
                  onClose={() => setAsk(null)}
                />
              )}
              {expanded &&
                (body && body.length > 0 ? (
                  <pre className="overflow-x-auto px-3 pb-3 font-mono text-[11.5px] leading-relaxed">
                    <div className="w-max min-w-full">
                      {body.map((line, i) => (
                        <div
                          key={i}
                          onClick={onAsk ? () => setAsk({ path: f.path, line }) : undefined}
                          title={onAsk ? t("diff.askLine") : undefined}
                          className={cn(
                            "whitespace-pre",
                            lineClass(line.text),
                            onAsk && "cursor-pointer hover:bg-brand-ghost/60",
                          )}
                        >
                          {line.text || " "}
                        </div>
                      ))}
                    </div>
                  </pre>
                ) : (
                  <p className="px-3 pb-3 pl-8 text-[11px] text-ink-faint">
                    {t("diff.untrackedOnly")}
                  </p>
                ))}
            </div>
          );
        })}
      </div>
      )}
    </div>
  );
}

/** Segmented-toggle button for the diff mode switch. */
function ModeBtn({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        "rounded-[var(--radius-sm)] px-2 py-0.5 text-[11px] font-medium transition-colors",
        active ? "bg-brand-ghost text-brand" : "text-ink-faint hover:text-ink",
      )}
    >
      {children}
    </button>
  );
}

type TargetMeta = { target: string; defaultBranch: string; resolved: string };

/** Editable per-task target branch for "vs target" mode: type a branch + ↵ to
 *  save (persisted on the direction), blank reverts to the repo default. The
 *  refresh button re-fetches origin/<target> for the latest remote state. */
function TargetEditor({
  directionId,
  meta,
  onChanged,
}: {
  directionId: number;
  meta: TargetMeta | null;
  onChanged: () => void;
}) {
  const { t } = useTranslation();
  const [val, setVal] = useState(meta?.target ?? "");
  // Keep the field in sync when the backend value arrives/changes, but don't
  // clobber an edit in progress (only adopt when it matches the last loaded).
  const lastLoaded = useRef(meta?.target ?? "");
  useEffect(() => {
    const incoming = meta?.target ?? "";
    if (incoming !== lastLoaded.current) {
      lastLoaded.current = incoming;
      setVal(incoming);
    }
  }, [meta?.target]);

  const save = () => {
    const next = val.trim();
    if (next === (meta?.target ?? "").trim()) return; // unchanged
    lastLoaded.current = next;
    void api.setDirectionTargetBranch(directionId, next).then(onChanged).catch(() => {});
  };

  return (
    <div className="flex items-center gap-1.5 text-[11px] text-ink-faint">
      <span className="shrink-0">{t("diff.compareAgainst")}</span>
      <input
        value={val}
        onChange={(e) => setVal(e.target.value)}
        onBlur={save}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            (e.target as HTMLInputElement).blur();
          } else if (e.key === "Escape") {
            setVal(meta?.target ?? "");
            (e.target as HTMLInputElement).blur();
          }
        }}
        placeholder={meta?.defaultBranch || "main"}
        spellCheck={false}
        className="min-w-0 flex-1 rounded-[var(--radius-sm)] border border-border bg-bg px-2 py-0.5 font-mono text-[11px] text-ink outline-none focus:border-brand"
      />
      {meta?.resolved && (
        <span
          className="shrink-0 truncate font-mono text-[10.5px] text-ink-faint/70"
          title={meta.resolved}
        >
          {meta.resolved}
        </span>
      )}
      <Tooltip label={t("diff.refreshTarget")}>
        <button
          onClick={onChanged}
          aria-label={t("diff.refreshTarget")}
          className="grid h-6 w-6 shrink-0 place-items-center rounded text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
        >
          <RefreshCw size={12} />
        </button>
      </Tooltip>
    </div>
  );
}

/** True on macOS — picks the right modifier glyph for the send shortcut. */
const isMac =
  typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);

/**
 * The in-place annotation composer: a review-comment block anchored to a file
 * or a single diff line. Header names the target (real new/old line number when
 * known), a multi-line field collects the change request, ⌘/Ctrl+↵ sends it to
 * the worker (Enter inserts a newline; Esc cancels).
 */
function AskBox({
  path,
  line,
  onSend,
  onClose,
}: {
  path: string;
  line?: DiffLine;
  onSend: (text: string) => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [q, setQ] = useState("");
  const ref = useRef<HTMLTextAreaElement>(null);
  useEffect(() => ref.current?.focus(), []);

  const quoted = line?.text != null && line.text.trim() !== "" ? line.text : null;
  // Which line this comment targets: the new-side number if the line exists in
  // the new file, else the old-side number, else null (whole file).
  const at =
    line?.rno != null
      ? { side: "R", n: line.rno }
      : line?.lno != null
        ? { side: "L", n: line.lno }
        : null;
  const target = at ? t("diff.askLineRef", at) : t("diff.askFileRef");

  const grow = (el: HTMLTextAreaElement) => {
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 160)}px`;
  };

  const send = () => {
    const v = q.trim();
    if (!v) return;
    // Carry the exact line target into the message so the worker applies the
    // request to the right line even when the quoted text repeats (dup/blank).
    const header = at
      ? t("diff.askContextLine", { path, ...at })
      : t("diff.askContext", { path });
    const quote = quoted != null ? `\n> ${quoted}` : "";
    onSend(`${header}${quote}\n\n${v}`);
  };

  return (
    <div className="mx-3 mb-2 rounded-[var(--radius-md)] border border-border bg-raised p-2.5">
      <div className="mb-2 flex items-center gap-1.5">
        <MessageSquarePlus size={13} className="shrink-0 text-brand" />
        <span className="text-[12px] font-semibold text-ink">{t("diff.askTitle")}</span>
        <span className="ml-auto truncate pl-2 font-mono text-[11px] text-ink-faint">
          {target}
        </span>
      </div>
      {quoted != null && (
        <div className="mb-2 truncate border-l-2 border-brand/50 pl-2 font-mono text-[11px] text-ink-muted">
          {quoted}
        </div>
      )}
      <textarea
        ref={ref}
        value={q}
        rows={2}
        onChange={(e) => {
          setQ(e.currentTarget.value);
          grow(e.currentTarget);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
            e.preventDefault();
            send();
          } else if (e.key === "Escape") {
            // Cancel just this annotation — don't let Escape bubble to
            // DiffPanel's window listener, which would close the whole panel.
            e.stopPropagation();
            onClose();
          }
        }}
        placeholder={t("diff.askPlaceholder")}
        className="w-full resize-none rounded-[var(--radius-sm)] border border-border bg-bg px-2 py-1.5 text-[12px] leading-relaxed text-ink outline-none focus:border-brand/60"
      />
      <div className="mt-2 flex items-center gap-2">
        <span className="mr-auto text-[11px] text-ink-faint">
          {t("diff.askHint", { key: isMac ? "⌘↵" : "Ctrl+↵" })}
        </span>
        <button
          onClick={onClose}
          className="rounded-[var(--radius-sm)] px-2.5 py-1 text-[12px] text-ink-muted transition-colors hover:bg-surface hover:text-ink"
        >
          {t("diff.askCancel")}
        </button>
        <button
          onClick={send}
          disabled={!q.trim()}
          className="inline-flex items-center gap-1 rounded-[var(--radius-sm)] bg-brand px-3 py-1 text-[12px] font-medium text-brand-ink transition-opacity disabled:opacity-40"
        >
          <Send size={12} />
          {t("diff.askSubmit")}
        </button>
      </div>
    </div>
  );
}

/** One line of a unified-diff body, tagged with its old/new file line numbers. */
type DiffLine = {
  /** Raw patch line: "+added", "-removed", " context", or an "@@ … @@" header. */
  text: string;
  /** Old-file (left) line number — set for context and removed (`-`) lines. */
  lno?: number;
  /** New-file (right) line number — set for context and added (`+`) lines. */
  rno?: number;
};

/** Split a unified patch into per-file bodies, dropping git header noise and
 *  numbering each line by its position in the old and new files. */
function parsePatch(patch: string): Record<string, DiffLine[]> {
  const out: Record<string, DiffLine[]> = {};
  let cur: string | null = null;
  let buf: DiffLine[] = [];
  let oldNo = 0;
  let newNo = 0;
  let inHunk = false;
  const flush = () => {
    if (cur) out[cur] = buf;
  };
  for (const line of patch.split("\n")) {
    if (line.startsWith("diff --git")) {
      flush();
      buf = [];
      oldNo = 0;
      newNo = 0;
      inHunk = false;
      const m = line.match(/ b\/(.+)$/);
      cur = m ? m[1] : line;
    } else if (
      !inHunk &&
      (line.startsWith("index ") ||
        line.startsWith("--- ") ||
        line.startsWith("+++ ") ||
        line.startsWith("new file") ||
        line.startsWith("deleted file") ||
        line.startsWith("old mode") ||
        line.startsWith("new mode") ||
        line.startsWith("similarity ") ||
        line.startsWith("rename "))
    ) {
      // file-level metadata, which only appears before the first hunk — drop it;
      // the section header already names the file. Inside a hunk, lines like
      // "+++ x" / "--- x" are real added/removed content, not headers.
    } else if (cur) {
      if (line.startsWith("@@")) {
        // hunk header resets the counters: @@ -oldStart,_ +newStart,_ @@
        const m = line.match(/@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
        if (m) {
          oldNo = Number(m[1]);
          newNo = Number(m[2]);
          inHunk = true;
        }
        buf.push({ text: line });
      } else if (!inHunk) {
        // pre-hunk metadata (e.g. "Binary files … differ") — owns no line number
        buf.push({ text: line });
      } else if (line.startsWith("+")) {
        buf.push({ text: line, rno: newNo });
        newNo++;
      } else if (line.startsWith("-")) {
        buf.push({ text: line, lno: oldNo });
        oldNo++;
      } else if (line === "" || line.startsWith("\\")) {
        // blank tail line / "\ No newline at end of file" — owns no line number
        buf.push({ text: line });
      } else {
        // context line — advances both files
        buf.push({ text: line, lno: oldNo, rno: newNo });
        oldNo++;
        newNo++;
      }
    }
  }
  flush();
  return out;
}

function lineClass(line: string): string {
  if (line.startsWith("@@")) return "text-brand";
  if (line.startsWith("+")) return "bg-running/10 text-running";
  if (line.startsWith("-")) return "bg-[oklch(0.64_0.2_25/0.1)] text-danger";
  return "text-ink-muted";
}
