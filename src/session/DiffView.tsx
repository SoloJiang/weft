import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChevronDown, ChevronRight, FileText } from "lucide-react";
import { api } from "../lib/api";
import type { WorktreeDiff } from "../lib/types";
import { cn } from "../lib/cn";

/**
 * Worker review surface: the worktree's net git diff (file stats + unified
 * patch), polled live, as collapsible per-file sections. Lets you see exactly
 * what the worker changed without dropping into the terminal.
 */
export function DiffView({ cwd }: { cwd: string }) {
  const { t } = useTranslation();
  const [diff, setDiff] = useState<WorktreeDiff | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [open, setOpen] = useState<Record<string, boolean>>({});
  const [touched, setTouched] = useState(false);

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      try {
        const d = await api.worktreeDiff(cwd);
        if (alive) {
          setDiff(d);
          setLoaded(true);
        }
      } catch {
        /* not ready */
      }
    };
    void tick();
    const h = setInterval(tick, 3000);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, [cwd]);

  const bodyByPath = useMemo(() => parsePatch(diff?.patch ?? ""), [diff?.patch]);

  if (loaded && diff && diff.files.length === 0) {
    return (
      <div className="flex flex-1 items-center justify-center px-6 text-center">
        <p className="text-[12px] leading-relaxed text-ink-faint">{t("diff.empty")}</p>
      </div>
    );
  }

  const files = diff?.files ?? [];
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
    <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
      <div className="sticky top-0 z-10 flex items-center gap-2 border-b border-border bg-bg/95 px-4 py-2.5 text-[11px] text-ink-faint backdrop-blur">
        <span>{t("diff.filesChanged", { count: files.length })}</span>
        <span className="text-running">+{totalAdded}</span>
        <span className="text-danger">−{totalRemoved}</span>
      </div>

      <div className="flex flex-col">
        {files.map((f) => {
          const body = bodyByPath[f.path];
          const expanded = isOpen(f.path);
          return (
            <div key={f.path} className="border-b border-border/60">
              <button
                onClick={() => toggle(f.path)}
                className="flex w-full items-center gap-2 px-3 py-2 text-left transition-colors hover:bg-surface"
              >
                {expanded ? (
                  <ChevronDown size={13} className="shrink-0 text-ink-faint" />
                ) : (
                  <ChevronRight size={13} className="shrink-0 text-ink-faint" />
                )}
                <FileText size={12} className="shrink-0 text-ink-faint" />
                <span className="truncate font-mono text-[12px] text-ink">{f.path}</span>
                <span className="ml-auto shrink-0 tabular-nums text-[11px]">
                  <span className="text-running">+{f.added}</span>{" "}
                  <span className="text-danger">−{f.removed}</span>
                </span>
              </button>
              {expanded &&
                (body && body.length > 0 ? (
                  <pre className="overflow-x-auto px-3 pb-3 font-mono text-[11.5px] leading-relaxed">
                    {body.map((line, i) => (
                      <div key={i} className={cn("whitespace-pre", lineClass(line))}>
                        {line || " "}
                      </div>
                    ))}
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
    </div>
  );
}

/** Split a unified patch into per-file bodies, dropping git header noise. */
function parsePatch(patch: string): Record<string, string[]> {
  const out: Record<string, string[]> = {};
  let cur: string | null = null;
  let buf: string[] = [];
  const flush = () => {
    if (cur) out[cur] = buf;
  };
  for (const line of patch.split("\n")) {
    if (line.startsWith("diff --git")) {
      flush();
      buf = [];
      const m = line.match(/ b\/(.+)$/);
      cur = m ? m[1] : line;
    } else if (
      line.startsWith("index ") ||
      line.startsWith("--- ") ||
      line.startsWith("+++ ") ||
      line.startsWith("new file") ||
      line.startsWith("deleted file") ||
      line.startsWith("old mode") ||
      line.startsWith("new mode") ||
      line.startsWith("similarity ") ||
      line.startsWith("rename ")
    ) {
      // header noise — the section header already names the file
    } else if (cur) {
      buf.push(line);
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
