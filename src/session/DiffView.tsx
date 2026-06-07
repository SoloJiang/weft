import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { FileText } from "lucide-react";
import { api } from "../lib/api";
import type { WorktreeDiff } from "../lib/types";
import { cn } from "../lib/cn";

/**
 * Worker review surface: the worktree's net git diff (file stats + unified
 * patch), polled live. Lets you see exactly what the worker changed without
 * dropping into the terminal.
 */
export function DiffView({ cwd }: { cwd: string }) {
  const { t } = useTranslation();
  const [diff, setDiff] = useState<WorktreeDiff | null>(null);
  const [loaded, setLoaded] = useState(false);

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

  if (loaded && diff && diff.files.length === 0) {
    return (
      <div className="flex flex-1 items-center justify-center px-6 text-center">
        <p className="text-[12px] leading-relaxed text-ink-faint">{t("diff.empty")}</p>
      </div>
    );
  }

  const totalAdded = diff?.files.reduce((s, f) => s + f.added, 0) ?? 0;
  const totalRemoved = diff?.files.reduce((s, f) => s + f.removed, 0) ?? 0;

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
      {/* file stat summary */}
      <div className="sticky top-0 z-10 flex flex-col gap-1 border-b border-border bg-bg/95 px-4 py-3 backdrop-blur">
        <div className="flex items-center gap-2 text-[11px] text-ink-faint">
          <span>{t("diff.filesChanged", { count: diff?.files.length ?? 0 })}</span>
          <span className="text-running">+{totalAdded}</span>
          <span className="text-danger">−{totalRemoved}</span>
        </div>
        <ul className="flex flex-col gap-0.5">
          {diff?.files.map((f) => (
            <li key={f.path} className="flex items-center gap-2 text-[12px]">
              <FileText size={12} className="shrink-0 text-ink-faint" />
              <span className="truncate text-ink-muted">{f.path}</span>
              <span className="ml-auto shrink-0 tabular-nums text-[11px]">
                <span className="text-running">+{f.added}</span>{" "}
                <span className="text-danger">−{f.removed}</span>
              </span>
            </li>
          ))}
        </ul>
      </div>

      {/* unified patch with line coloring */}
      {diff?.patch ? (
        <pre className="flex-1 overflow-x-auto px-4 py-3 font-mono text-[11.5px] leading-relaxed">
          {diff.patch.split("\n").map((line, i) => (
            <div key={i} className={cn("whitespace-pre", lineClass(line))}>
              {line || " "}
            </div>
          ))}
        </pre>
      ) : (
        diff &&
        diff.files.length > 0 && (
          <p className="px-4 py-3 text-[11px] text-ink-faint">{t("diff.untrackedOnly")}</p>
        )
      )}
    </div>
  );
}

function lineClass(line: string): string {
  if (line.startsWith("diff --git") || line.startsWith("index ")) return "text-ink-faint/60";
  if (line.startsWith("@@")) return "text-brand";
  if (line.startsWith("+++") || line.startsWith("---")) return "text-ink-faint";
  if (line.startsWith("+")) return "bg-running/10 text-running";
  if (line.startsWith("-")) return "bg-[oklch(0.64_0.2_25/0.1)] text-danger";
  return "text-ink-muted";
}
